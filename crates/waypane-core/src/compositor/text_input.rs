use super::dispatch::{bad, object_id, rect, string, uint};
use super::*;
use crate::protocol::text_input as ime;

const MAX_TEXT_BUFFER_BYTES: usize = 4000;
const CONTENT_HINT_MASK: u32 = 0x3ff;
const MAX_CONTENT_PURPOSE: u32 = 13;

impl Compositor {
    pub fn commit_text(&mut self, window_id: WindowId, text: &str) -> Result<usize> {
        validate_text(text)?;
        let mut count = 0;
        for (id, serial) in self.ime_targets(window_id) {
            self.emit(
                &ime::ZWP_TEXT_INPUT_V3_INTERFACE,
                "preedit_string",
                id,
                &[Argument::Str(None), Argument::Int(-1), Argument::Int(-1)],
            );
            self.emit(
                &ime::ZWP_TEXT_INPUT_V3_INTERFACE,
                "commit_string",
                id,
                &[Argument::Str(Some(text))],
            );
            self.emit(
                &ime::ZWP_TEXT_INPUT_V3_INTERFACE,
                "done",
                id,
                &[Argument::Uint(serial)],
            );
            count += 1;
        }
        Ok(count)
    }

    pub fn set_preedit_text(
        &mut self,
        window_id: WindowId,
        text: Option<&str>,
        cursor_begin: i32,
        cursor_end: i32,
    ) -> Result<usize> {
        let (text, cursor_begin, cursor_end) = match text {
            Some(text) => {
                validate_text(text)?;
                let len = i32::try_from(text.len()).unwrap_or(i32::MAX);
                if !(0..=len).contains(&cursor_begin)
                    || !(0..=len).contains(&cursor_end)
                    || cursor_begin > cursor_end
                {
                    return Err(Error::protocol(format!(
                        "invalid preedit cursor range {cursor_begin}..{cursor_end} for {len} bytes"
                    )));
                }
                (Some(text), cursor_begin, cursor_end)
            }
            None => (None, -1, -1),
        };
        let mut count = 0;
        for (id, serial) in self.ime_targets(window_id) {
            self.emit(
                &ime::ZWP_TEXT_INPUT_V3_INTERFACE,
                "preedit_string",
                id,
                &[
                    Argument::Str(text),
                    Argument::Int(cursor_begin),
                    Argument::Int(cursor_end),
                ],
            );
            self.emit(
                &ime::ZWP_TEXT_INPUT_V3_INTERFACE,
                "done",
                id,
                &[Argument::Uint(serial)],
            );
            count += 1;
        }
        Ok(count)
    }

    pub fn delete_surrounding_text(
        &mut self,
        window_id: WindowId,
        before_length: u32,
        after_length: u32,
    ) -> usize {
        let targets = self.ime_targets(window_id);
        for (id, serial) in &targets {
            self.emit(
                &ime::ZWP_TEXT_INPUT_V3_INTERFACE,
                "delete_surrounding_text",
                *id,
                &[Argument::Uint(before_length), Argument::Uint(after_length)],
            );
            self.emit(
                &ime::ZWP_TEXT_INPUT_V3_INTERFACE,
                "done",
                *id,
                &[Argument::Uint(*serial)],
            );
        }
        targets.len()
    }

    pub(super) fn create_text_input(&mut self, request: &DecodedRequest) -> Result<()> {
        let id = self.create_new(request, 0)?;
        let seat_id = object_id(request, 1)?;
        if !self.seats.contains_key(&seat_id) {
            return Err(bad(request, "unknown wl_seat for text input"));
        }
        let surface_id = self
            .keyboard_focus
            .and_then(|window| self.windows.get(&window))
            .map(|window| window.surface_id);
        let focused = surface_id.is_some();
        self.text_inputs.insert(
            id,
            TextInput {
                seat_id,
                surface_id,
                focused,
                ..TextInput::default()
            },
        );
        if let Some(surface_id) = surface_id {
            self.emit(
                &ime::ZWP_TEXT_INPUT_V3_INTERFACE,
                "enter",
                id,
                &[Argument::Object(surface_id)],
            );
        }
        Ok(())
    }

    pub(super) fn sync_text_input_focus(
        &mut self,
        old_surface: Option<u32>,
        new_surface: Option<u32>,
    ) -> Result<()> {
        if old_surface == new_surface {
            return Ok(());
        }
        let ids = self.text_inputs.keys().copied().collect::<Vec<_>>();
        for id in ids {
            let Some(input) = self.text_inputs.get_mut(&id) else {
                continue;
            };
            let current_surface = input.surface_id;
            if current_surface == new_surface {
                continue;
            }
            let was_enabled = input.enabled;
            let content_hint = input.content_hint;
            let content_purpose = input.content_purpose;
            let cursor_rect = input.cursor_rect;
            input.surface_id = new_surface;
            input.focused = new_surface.is_some();
            input.enabled = false;
            input.pending = PendingTextInput::default();
            if let Some(old) = current_surface {
                self.emit(
                    &ime::ZWP_TEXT_INPUT_V3_INTERFACE,
                    "leave",
                    id,
                    &[Argument::Object(old)],
                );
                if was_enabled {
                    self.events.push(CoreEvent::TextInputState {
                        surface_id: old,
                        enabled: false,
                        content_hint,
                        content_purpose,
                        cursor_rect,
                    });
                }
            }
            if let Some(new) = new_surface {
                self.emit(
                    &ime::ZWP_TEXT_INPUT_V3_INTERFACE,
                    "enter",
                    id,
                    &[Argument::Object(new)],
                );
            }
        }
        Ok(())
    }

    pub(super) fn handle_text_input_request(&mut self, request: &DecodedRequest) -> Result<()> {
        let other_enabled = request.method.name == "commit"
            && self.text_inputs.iter().any(|(id, input)| {
                *id != request.object_id
                    && input.enabled
                    && self
                        .text_inputs
                        .get(&request.object_id)
                        .is_some_and(|current| input.seat_id == current.seat_id)
            });
        let Some(input) = self.text_inputs.get_mut(&request.object_id) else {
            return Err(bad(request, "unknown zwp_text_input_v3"));
        };
        if !input.focused {
            return Ok(());
        }
        match request.method.name {
            "enable" => input.pending.enabled = Some(true),
            "disable" => input.pending.enabled = Some(false),
            "set_surrounding_text" => {
                let text = string(request, 0)?.to_owned();
                if text.len() > MAX_TEXT_BUFFER_BYTES {
                    return Err(bad(request, "surrounding text exceeds 4000 bytes"));
                }
                let cursor = uint(request, 1)? as usize;
                let anchor = uint(request, 2)? as usize;
                if cursor > text.len() || anchor > text.len() {
                    return Err(bad(request, "surrounding text cursor exceeds text length"));
                }
                input.pending.surrounding_text = Some(text);
            }
            "set_content_type" => {
                let hint = uint(request, 0)?;
                let purpose = uint(request, 1)?;
                if hint & !CONTENT_HINT_MASK != 0 || purpose > MAX_CONTENT_PURPOSE {
                    return Err(bad(request, "invalid text content type"));
                }
                input.pending.content_hint = Some(hint);
                input.pending.content_purpose = Some(purpose);
            }
            "set_cursor_rectangle" => {
                let rect = rect(request, 0)?;
                if rect.width < 0 || rect.height < 0 {
                    return Err(bad(request, "negative cursor rectangle size"));
                }
                input.pending.cursor_rect = Some(rect);
            }
            "set_text_change_cause" => {}
            "commit" => {
                input.commit_count = input.commit_count.wrapping_add(1);
                let pending = std::mem::take(&mut input.pending);
                if let Some(enabled) = pending.enabled {
                    if enabled && other_enabled {
                        input.enabled = false;
                    } else {
                        input.enabled = enabled;
                    }
                }
                if let Some(hint) = pending.content_hint {
                    input.content_hint = hint;
                }
                if let Some(purpose) = pending.content_purpose {
                    input.content_purpose = purpose;
                }
                if let Some(text) = pending.surrounding_text {
                    input.surrounding_text = Some(text);
                }
                if let Some(rect) = pending.cursor_rect {
                    input.cursor_rect = rect;
                }
                let enabled = input.enabled;
                let surface_id = input.surface_id.unwrap_or_default();
                let serial = input.commit_count;
                let content_hint = input.content_hint;
                let content_purpose = input.content_purpose;
                let cursor_rect = input.cursor_rect;
                self.events.push(CoreEvent::TextInputState {
                    surface_id,
                    enabled,
                    content_hint,
                    content_purpose,
                    cursor_rect,
                });
                self.emit(
                    &ime::ZWP_TEXT_INPUT_V3_INTERFACE,
                    "done",
                    request.object_id,
                    &[Argument::Uint(serial)],
                );
            }
            "destroy" => {}
            other => {
                return Err(bad(
                    request,
                    format!("unsupported text input request {other}"),
                ))
            }
        }
        Ok(())
    }

    pub(super) fn finish_text_input_destructor(&mut self, id: u32) {
        self.text_inputs.remove(&id);
    }

    fn ime_targets(&self, window_id: WindowId) -> Vec<(u32, u32)> {
        let Some(surface_id) = self.windows.get(&window_id).map(|window| window.surface_id) else {
            return Vec::new();
        };
        self.text_inputs
            .iter()
            .filter(|(_, input)| {
                input.enabled && input.focused && input.surface_id == Some(surface_id)
            })
            .map(|(id, input)| (*id, input.commit_count))
            .collect()
    }
}

fn validate_text(text: &str) -> Result<()> {
    if text.len() > MAX_TEXT_BUFFER_BYTES {
        return Err(Error::protocol("text input exceeds 4000 bytes"));
    }
    Ok(())
}
