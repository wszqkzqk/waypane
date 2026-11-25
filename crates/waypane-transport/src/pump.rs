// Waypane: window-level Wayland forwarding over SSH.
// Copyright (C) 2026  Zhou Qiankang <wszqkzqk@qq.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::io;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

pub async fn pump<S>(
    stream: S,
    inbound: mpsc::Sender<Bytes>,
    mut outbound: mpsc::Receiver<Bytes>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut buffer = [0u8; 65536];
    loop {
        tokio::select! {
            result = reader.read(&mut buffer) => {
                match result? {
                    0 => return Ok(()),
                    count => {
                        if inbound.send(Bytes::copy_from_slice(&buffer[..count])).await.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
            packet = outbound.recv() => {
                match packet {
                    Some(packet) => {
                        writer.write_all(&packet).await?;
                        writer.flush().await?;
                    }
                    None => {
                        writer.shutdown().await?;
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pumps_both_directions_with_backpressure_channels() {
        let (client, server) = tokio::io::duplex(4096);
        let (inbound_tx, mut inbound_rx) = mpsc::channel(1);
        let (outbound_tx, outbound_rx) = mpsc::channel(1);
        let task = tokio::spawn(pump(server, inbound_tx, outbound_rx));
        let (mut client_reader, mut client_writer) = tokio::io::split(client);
        let (result_tx, mut result_rx) = mpsc::channel(1);

        let writer = tokio::spawn(async move {
            client_writer.write_all(b"request").await.unwrap();
            let count = result_rx.recv().await.unwrap();
            client_writer.write_all(&[0u8; 8][..count]).await.unwrap();
        });
        assert_eq!(
            inbound_rx.recv().await.unwrap(),
            Bytes::from_static(b"request")
        );
        outbound_tx
            .send(Bytes::from_static(b"response"))
            .await
            .unwrap();
        let mut response = [0u8; 8];
        client_reader.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"response");
        result_tx.send(0usize).await.unwrap();
        writer.await.unwrap();
        drop(outbound_tx);
        task.await.unwrap().unwrap();
    }
}
