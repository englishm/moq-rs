// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{io::Cursor, sync::Arc};

use anyhow::Context;
use moq_transport::serve::{
    SubgroupObjectReader, SubgroupReader, TrackReader, TrackReaderMode, Tracks, TracksReader,
    TracksWriter,
};
use moq_transport::session::Subscriber;
use mp4::ReadBox;
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
    task::JoinSet,
};
use tracing::{debug, info, trace, warn};

pub struct Media<O> {
    subscriber: Subscriber,
    broadcast: TracksReader,
    tracks_writer: TracksWriter,
    output: Arc<Mutex<O>>,
    request_catalog: bool,
}

impl<O: AsyncWrite + Send + Unpin + 'static> Media<O> {
    pub async fn new(
        subscriber: Subscriber,
        tracks: Tracks,
        output: O,
        request_catalog: bool,
    ) -> anyhow::Result<Self> {
        let (tracks_writer, _tracks_request, tracks_reader) = tracks.produce();
        let broadcast = tracks_reader; // breadcrumb for navigating API name changes
        Ok(Self {
            subscriber,
            broadcast,
            tracks_writer,
            output: Arc::new(Mutex::new(output)),
            request_catalog,
        })
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        let catalog = if self.request_catalog {
            // The catalog track has no standardized name, but
            // both moq-pub of moq-rs and gst-moq-pub uses ".catalog".
            let buf = self.download_first_object(".catalog", "catalog").await?;
            let s = std::str::from_utf8(&buf)?;
            let c: moq_catalog::Root = serde_json::from_str(s)?;
            info!("catalog: {c:#?}");
            anyhow::ensure!(c.version == 1, "Unknown catalog version");
            Some(c)
        } else {
            None
        };
        let moov = {
            let init_track_name: &str = match catalog {
                Some(ref c) => &c.tracks[0].init_track.clone().unwrap(),
                None => "0.mp4",
            };
            let buf = self.download_first_object(init_track_name, "init").await?;
            self.output.lock().await.write_all(&buf).await?;
            let mut reader = Cursor::new(&buf);

            let ftyp = read_atom(&mut reader).await?;
            anyhow::ensure!(&ftyp[4..8] == b"ftyp", "expected ftyp atom");

            let moov = read_atom(&mut reader).await?;
            anyhow::ensure!(&moov[4..8] == b"moov", "expected moov atom");
            let mut moov_reader = Cursor::new(&moov);
            let moov_header = mp4::BoxHeader::read(&mut moov_reader)?;

            mp4::MoovBox::read_box(&mut moov_reader, moov_header.size)?
        };

        let mut has_video = false;
        let mut has_audio = false;
        let mut tracks = vec![];
        for (idx, trak) in moov.traks.into_iter().enumerate() {
            let id = trak.tkhd.track_id;
            let name: String = match catalog {
                Some(ref c) => c.tracks[idx].name.clone(),
                None => format!("{id}.m4s"),
            };
            info!("found track {name}");
            let mut active = false;
            if !has_video && trak.mdia.minf.stbl.stsd.avc1.is_some() {
                active = true;
                has_video = true;
                info!("using {name} for video");
            }
            if !has_audio && trak.mdia.minf.stbl.stsd.mp4a.is_some() {
                active = true;
                has_audio = true;
                info!("using {name} for audio");
            }
            if active {
                let track = self
                    .tracks_writer
                    .create(&name)
                    .context("failed to create track")?;

                let mut subscriber = self.subscriber.clone();
                tokio::task::spawn(async move {
                    subscriber.subscribe(track).await.unwrap_or_else(|err| {
                        warn!("failed to subscribe to track: {err:?}");
                    });
                });

                // The track was just created locally by `tracks_writer.create`,
                // so this is a plain cache hit with no pull-through interest to
                // register (hence no guard to hold).
                let (track_reader, _interest) = self
                    .broadcast
                    .subscribe(self.broadcast.namespace.clone(), &name)
                    .context("no track")?;
                tracks.push(track_reader);
            }
        }

        info!("playing {} tracks", tracks.len());
        let mut tasks = JoinSet::new();
        for track in tracks {
            let out = self.output.clone();
            tasks.spawn(async move {
                let name = track.name.clone();
                if let Err(err) = Self::recv_track(track, out).await {
                    warn!("failed to play track {name}: {err:?}");
                }
            });
        }
        while tasks.join_next().await.is_some() {}
        Ok(())
    }

    async fn download_first_object(
        &mut self,
        track_name: &str,
        alias: &'static str,
    ) -> anyhow::Result<Vec<u8>> {
        let track = self
            .tracks_writer
            .create(track_name)
            .context(format!("failed to create {alias} track"))?;

        let mut subscriber = self.subscriber.clone();
        tokio::task::spawn(async move {
            subscriber.subscribe(track).await.unwrap_or_else(|err| {
                warn!("failed to subscribe to {alias} track: {err:?}");
            });
        });

        let (track, _interest) = self
            .broadcast
            .subscribe(self.broadcast.namespace.clone(), track_name)
            .context(format!("no {alias} track"))?;
        let mut group = match track.mode().await? {
            TrackReaderMode::Subgroups(mut groups) => {
                groups.next().await?.context(format!("no {alias} group"))?
            }
            _ => anyhow::bail!("expected {alias} segment"),
        };

        let object = group
            .next()
            .await?
            .context(format!("no {alias} fragment"))?;
        let buf = Self::recv_object(object).await?;
        Ok(buf)
    }

    async fn recv_track(track: TrackReader, out: Arc<Mutex<O>>) -> anyhow::Result<()> {
        let name = track.name.clone();
        debug!("track {name}: start");
        if let TrackReaderMode::Subgroups(mut groups) = track.mode().await? {
            while let Some(group) = groups.next().await? {
                let out = out.clone();
                if let Err(err) = Self::recv_group(group, out).await {
                    warn!("failed to receive group: {err:?}");
                }
            }
        }
        debug!("track {name}: finish");
        Ok(())
    }

    async fn recv_group(mut group: SubgroupReader, out: Arc<Mutex<O>>) -> anyhow::Result<()> {
        trace!("group={} start", group.group_id);
        while let Some(object) = group.next().await? {
            trace!(
                "group={} fragment={} start",
                group.group_id,
                object.object_id
            );
            let out = out.clone();
            let buf = Self::recv_object(object).await?;

            out.lock().await.write_all(&buf).await?;
        }

        Ok(())
    }

    async fn recv_object(mut object: SubgroupObjectReader) -> anyhow::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(object.size);
        while let Some(chunk) = object.read().await? {
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }
}

// Read a full MP4 atom into a vector.
async fn read_atom<R: AsyncReadExt + Unpin>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
    // Read the 8 bytes for the size + type
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf).await?;

    // Convert the first 4 bytes into the size.
    let size = u32::from_be_bytes(buf[0..4].try_into()?) as u64;

    let mut raw = buf.to_vec();

    let mut limit = match size {
        // Runs until the end of the file.
        0 => reader.take(u64::MAX),

        // The next 8 bytes are the extended size to be used instead.
        1 => {
            reader.read_exact(&mut buf).await?;
            let size_large = u64::from_be_bytes(buf);
            anyhow::ensure!(
                size_large >= 16,
                "impossible extended box size: {}",
                size_large
            );

            reader.take(size_large - 16)
        }

        2..=7 => {
            anyhow::bail!("impossible box size: {}", size)
        }

        size => reader.take(size - 8),
    };

    // Append to the vector and return it.
    let _read_bytes = limit.read_to_end(&mut raw).await?;

    Ok(raw)
}
