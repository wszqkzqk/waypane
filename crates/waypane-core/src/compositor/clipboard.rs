use std::sync::Arc;

use waypipe_client::PipeOpenKind;

use super::dispatch::{bad, fd, object_id, optional_object_id, string};

use super::*;

const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;
const TEXT_MIME_TYPES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

impl Compositor {
    pub fn set_clipboard_text(&mut self, text: &str) -> Result<()> {
        let mime_types = TEXT_MIME_TYPES
            .iter()
            .map(|mime| (*mime).to_owned())
            .collect::<Vec<_>>();
        self.selection = Some(SelectionState::Host {
            text: Arc::from(text.as_bytes()),
            mime_types: mime_types.clone(),
        });
        self.refresh_clipboard_offers()?;
        self.events.push(CoreEvent::ClipboardChanged {
            source: None,
            mime_types,
        });
        Ok(())
    }

    pub fn clear_clipboard(&mut self) -> Result<()> {
        self.selection = None;
        self.refresh_clipboard_offers()?;
        self.events.push(CoreEvent::ClipboardChanged {
            source: None,
            mime_types: Vec::new(),
        });
        Ok(())
    }

    pub fn request_clipboard_text(&mut self, client: &mut Client) -> Result<Option<Rid>> {
        let Some(selection) = self.selection.clone() else {
            return Ok(None);
        };
        match selection {
            SelectionState::Host {
                text, mime_types, ..
            } => {
                let mime_type = text_mime(&mime_types)
                    .unwrap_or_else(|| "text/plain;charset=utf-8".to_string());
                self.events.push(CoreEvent::ClipboardData {
                    mime_type,
                    data: text.to_vec(),
                });
                Ok(None)
            }
            SelectionState::Remote {
                source_id,
                mime_types,
            } => {
                let mime_type = text_mime(&mime_types)
                    .ok_or_else(|| Error::protocol("current selection has no text MIME type"))?;
                let rid = self.begin_clipboard_read(client, source_id, &mime_type)?;
                Ok(Some(rid))
            }
        }
    }

    pub fn process_pipe(&mut self, client: &mut Client, rid: Rid) -> Result<Vec<CoreEvent>> {
        if !self.clipboard_reads.contains_key(&rid) {
            return Ok(vec![CoreEvent::PipeAvailable { rid }]);
        }
        let eof = loop {
            let read = client.resources_mut().read_pipe(rid, 256 * 1024)?;
            if read.data.is_empty() {
                break read.eof;
            }
            let chunk = read.data;
            let read_eof = read.eof;
            let pending = self.clipboard_reads.get_mut(&rid).unwrap();
            if pending.data.len().saturating_add(chunk.len()) > MAX_CLIPBOARD_BYTES {
                self.clipboard_reads.remove(&rid);
                client.resources_mut().close_resource(rid);
                return Err(Error::protocol("clipboard transfer exceeds 16 MiB limit"));
            }
            pending.data.extend_from_slice(&chunk);
            if read_eof {
                break true;
            }
        };
        if !eof {
            return Ok(Vec::new());
        }

        let read = self.clipboard_reads.remove(&rid).unwrap();
        if read.source_id.is_some() && is_text_mime(&read.mime_type) {
            if let Some(source) = read.source_id.and_then(|id| self.data_sources.get_mut(&id)) {
                source.cached_text = Some(Arc::from(read.data.clone()));
            }
        }
        for forward in read.forwards {
            self.write_clipboard_to_pipe(client, forward, &read.data)?;
        }
        client.resources_mut().close_resource(rid);
        let events = vec![CoreEvent::ClipboardData {
            mime_type: read.mime_type,
            data: read.data,
        }];
        Ok(events)
    }

    pub(super) fn create_data_source(&mut self, request: &DecodedRequest) -> Result<()> {
        let id = self.create_new(request, 0)?;
        self.data_sources.insert(id, DataSource::default());
        Ok(())
    }

    pub(super) fn create_data_device(&mut self, request: &DecodedRequest) -> Result<()> {
        let id = self.create_new(request, 0)?;
        let seat_id = object_id(request, 1)?;
        let Some(seat) = self.seats.get_mut(&seat_id) else {
            return Err(bad(request, "unknown wl_seat for data_device"));
        };
        seat.data_devices.push(id);
        let focused = self.keyboard_focus.is_some();
        self.data_devices.insert(
            id,
            DataDevice {
                focused,
                offered: Default::default(),
            },
        );
        if focused {
            self.offer_clipboard(id)?;
        }
        Ok(())
    }

    pub(super) fn offer_source_mime(&mut self, request: &DecodedRequest) -> Result<()> {
        let mime = string(request, 0)?.to_owned();
        if mime.len() > 1024 || mime.contains('\0') {
            return Err(bad(request, "invalid MIME type"));
        }
        let source = self
            .data_sources
            .get_mut(&request.object_id)
            .ok_or_else(|| bad(request, "unknown wl_data_source"))?;
        if !source.mime_types.contains(&mime) {
            source.mime_types.push(mime);
        }
        Ok(())
    }

    pub(super) fn set_remote_selection(&mut self, request: &DecodedRequest) -> Result<()> {
        if self.keyboard_focus.is_none() {
            return Err(bad(request, "set_selection requires keyboard focus"));
        }
        let source = optional_object_id(request, 0)?;
        let Some(source_id) = source else {
            self.selection = None;
            self.refresh_clipboard_offers()?;
            self.events.push(CoreEvent::ClipboardChanged {
                source: None,
                mime_types: Vec::new(),
            });
            return Ok(());
        };
        let source = self
            .data_sources
            .get(&source_id)
            .ok_or_else(|| bad(request, "unknown wl_data_source"))?;
        let mime_types = source.mime_types.clone();
        self.selection = Some(SelectionState::Remote {
            source_id,
            mime_types: mime_types.clone(),
        });
        self.refresh_clipboard_offers()?;
        self.events.push(CoreEvent::ClipboardChanged {
            source: Some(source_id),
            mime_types,
        });
        Ok(())
    }

    pub(super) fn receive_offer(
        &mut self,
        client: &mut Client,
        request: &DecodedRequest,
    ) -> Result<()> {
        let mime_type = string(request, 0)?.to_owned();
        let offer_id = request.object_id;
        let write_rid = fd(request, 1)?;
        let selection = self
            .offers
            .get(&offer_id)
            .cloned()
            .ok_or_else(|| bad(request, "unknown wl_data_offer"))?;
        if !is_text_mime(&mime_type) {
            return Err(bad(request, "only text clipboard transfers are supported"));
        }

        match selection {
            SelectionState::Host { text, .. } => {
                self.write_clipboard_to_pipe(client, write_rid, &text)
            }
            SelectionState::Remote { source_id, .. } => {
                let cached = self
                    .data_sources
                    .get(&source_id)
                    .and_then(|source| source.cached_text.clone());
                if let Some(text) = cached {
                    self.write_clipboard_to_pipe(client, write_rid, &text)
                } else {
                    let read_rid = self.begin_clipboard_read(client, source_id, &mime_type)?;
                    self.clipboard_reads
                        .get_mut(&read_rid)
                        .unwrap()
                        .forwards
                        .push(write_rid);
                    Ok(())
                }
            }
        }
    }

    pub(super) fn sync_data_device_focus(&mut self) -> Result<()> {
        let has_focus = self.keyboard_focus.is_some();
        let devices = self.data_devices.keys().copied().collect::<Vec<_>>();
        for id in devices {
            let was_focused = self
                .data_devices
                .get(&id)
                .is_some_and(|device| device.focused);
            if was_focused != has_focus {
                self.data_devices.get_mut(&id).unwrap().focused = has_focus;
                if has_focus {
                    self.offer_clipboard(id)?;
                } else {
                    self.emit(
                        &wl::WL_DATA_DEVICE_INTERFACE,
                        "selection",
                        id,
                        &[Argument::Object(0)],
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) fn finish_clipboard_destructor(
        &mut self,
        client: &mut Client,
        id: u32,
        kind: ObjectKind,
    ) -> bool {
        match kind {
            ObjectKind::DataDevice => {
                self.data_devices.remove(&id);
                for seat in self.seats.values_mut() {
                    seat.data_devices.retain(|device| *device != id);
                }
                true
            }
            ObjectKind::DataSource => {
                self.data_sources.remove(&id);
                if matches!(self.selection, Some(SelectionState::Remote { source_id, .. }) if source_id == id)
                {
                    self.selection = None;
                    let _ = self.refresh_clipboard_offers();
                    self.events.push(CoreEvent::ClipboardChanged {
                        source: None,
                        mime_types: Vec::new(),
                    });
                }
                let reads = self
                    .clipboard_reads
                    .iter()
                    .filter(|(_, read)| read.source_id == Some(id))
                    .map(|(rid, _)| *rid)
                    .collect::<Vec<_>>();
                for rid in reads {
                    self.clipboard_reads.remove(&rid);
                    client.resources_mut().close_resource(rid);
                }
                true
            }
            ObjectKind::DataOffer => {
                self.offers.remove(&id);
                for device in self.data_devices.values_mut() {
                    device.offered.remove(&id);
                }
                true
            }
            _ => false,
        }
    }

    fn begin_clipboard_read(
        &mut self,
        client: &mut Client,
        source_id: u32,
        mime_type: &str,
    ) -> Result<Rid> {
        let rid = client.create_pipe(PipeOpenKind::IR)?;
        self.clipboard_reads.insert(
            rid,
            ClipboardRead {
                mime_type: mime_type.to_owned(),
                data: Vec::new(),
                forwards: Vec::new(),
                source_id: Some(source_id),
            },
        );
        self.outbound.push(event_with_fd(
            source_id,
            event_opcode(&wl::WL_DATA_SOURCE_INTERFACE, "send"),
            &[Argument::Str(Some(mime_type))],
            rid,
        ));
        Ok(rid)
    }

    fn write_clipboard_to_pipe(
        &mut self,
        client: &mut Client,
        rid: Rid,
        data: &[u8],
    ) -> Result<()> {
        if data.len() > MAX_CLIPBOARD_BYTES {
            return Err(Error::protocol("clipboard payload exceeds 16 MiB limit"));
        }
        client.write_pipe(rid, data)?;
        client.close_pipe_write(rid)?;
        client.resources_mut().close_resource(rid);
        Ok(())
    }

    fn refresh_clipboard_offers(&mut self) -> Result<()> {
        let focused = self
            .data_devices
            .iter()
            .filter(|(_, device)| device.focused)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in focused {
            self.offer_clipboard(id)?;
        }
        Ok(())
    }

    fn offer_clipboard(&mut self, device: u32) -> Result<()> {
        let Some(selection) = self.selection.clone() else {
            self.emit(
                &wl::WL_DATA_DEVICE_INTERFACE,
                "selection",
                device,
                &[Argument::Object(0)],
            );
            return Ok(());
        };
        let offer = self.create_server_object(&wl::WL_DATA_OFFER_INTERFACE)?;
        self.offers.insert(offer, selection.clone());
        self.data_devices
            .get_mut(&device)
            .unwrap()
            .offered
            .insert(offer);
        self.emit(
            &wl::WL_DATA_DEVICE_INTERFACE,
            "data_offer",
            device,
            &[Argument::Object(offer)],
        );
        for mime in selection.mime_types() {
            self.emit(
                &wl::WL_DATA_OFFER_INTERFACE,
                "offer",
                offer,
                &[Argument::Str(Some(mime))],
            );
        }
        self.emit(
            &wl::WL_DATA_DEVICE_INTERFACE,
            "selection",
            device,
            &[Argument::Object(offer)],
        );
        Ok(())
    }

    fn create_server_object(&mut self, interface: &'static Interface) -> Result<u32> {
        while self.objects.contains_key(&self.next_server_id) {
            self.next_server_id = self
                .next_server_id
                .checked_add(1)
                .ok_or_else(|| Error::protocol("server object id space exhausted"))?;
        }
        let id = self.next_server_id;
        self.next_server_id = self
            .next_server_id
            .checked_add(1)
            .ok_or_else(|| Error::protocol("server object id space exhausted"))?;
        self.objects
            .insert(id, ObjectRecord::new(interface, interface.version));
        Ok(id)
    }
}

fn is_text_mime(mime: &str) -> bool {
    TEXT_MIME_TYPES
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(mime))
        || mime.strip_prefix("text/").is_some()
}

fn text_mime(mimes: &[String]) -> Option<String> {
    TEXT_MIME_TYPES
        .iter()
        .find_map(|candidate| {
            mimes
                .iter()
                .find(|mime| mime.eq_ignore_ascii_case(candidate))
                .cloned()
        })
        .or_else(|| mimes.iter().find(|mime| mime.starts_with("text/")).cloned())
}
