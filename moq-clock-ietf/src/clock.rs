// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use anyhow::Context;
use moq_transport::serve::{
    Datagram, DatagramsReader, DatagramsWriter, StreamReader, Subgroup, SubgroupWriter,
    SubgroupsReader, SubgroupsWriter, TrackReader, TrackReaderMode,
};

use chrono::prelude::*;
use tokio::task;

/// Publishes the current time every second in the format "YYYY-MM-DD HH:MM:SS"
pub struct Publisher {
    track_subgroups_writer: Option<SubgroupsWriter>,
    track_datagrams_writer: Option<DatagramsWriter>,
}

impl Publisher {
    pub fn new(track_subgroups_writer: SubgroupsWriter) -> Self {
        Self {
            track_subgroups_writer: Some(track_subgroups_writer),
            track_datagrams_writer: None,
        }
    }

    pub fn new_datagram(track_datagrams_writer: DatagramsWriter) -> Self {
        Self {
            track_subgroups_writer: None,
            track_datagrams_writer: Some(track_datagrams_writer),
        }
    }

    /// Runs the publisher, sending the current time every second.  Creates a new group for each minute.
    pub async fn run(mut self) -> anyhow::Result<()> {
        let start = Utc::now();
        let mut now = start;

        // Just for fun, don't start at zero.
        let mut next_group_id = start.minute();

        // Create a new group for each minute.
        loop {
            let mut next: DateTime<Utc>;
            if let Some(track_subgroups_writer) = &mut self.track_subgroups_writer {
                let subgroup_writer = track_subgroups_writer
                    .create(Subgroup {
                        group_id: next_group_id as u64,
                        subgroup_id: 0,
                        priority: 0,
                    })
                    .context("failed to create minute segment")?;

                // Spawn a new task to handle sending the object every second
                tokio::spawn(async move {
                    if let Err(err) = Self::send_subgroup_objects(subgroup_writer, now).await {
                        tracing::warn!("failed to send minute: {:?}", err);
                    }
                });

                next = now + chrono::Duration::try_minutes(1).unwrap();
                next = next.with_second(0).unwrap().with_nanosecond(0).unwrap();
            } else if let Some(track_datagrams_writer) = &mut self.track_datagrams_writer {
                let time_str = now.format("%Y-%m-%d %H:%M:%S").to_string();
                track_datagrams_writer
                    .write(Datagram {
                        group_id: next_group_id as u64,
                        object_id: 0,
                        priority: 127,
                        status: moq_transport::data::ObjectStatus::NormalObject,
                        end_of_group: false,
                        payload: time_str.clone().into_bytes().into(),
                        extension_headers: Default::default(),
                    })
                    .context("failed to write datagram")?;

                println!("{}", time_str);

                next = now + chrono::Duration::try_seconds(1).unwrap();
                next = next.with_nanosecond(0).unwrap();
            } else {
                return Err(anyhow::anyhow!("no track writer available"));
            }

            next_group_id += 1;

            // Sleep until the start of the next minute (stream mode) or next second (datagram mode)
            let delay = (next - now).to_std().unwrap();
            tokio::time::sleep(delay).await;

            now = next; // just assume we didn't undersleep
        }
    }

    /// Sends the current time every second within a minute group.
    async fn send_subgroup_objects(
        mut subgroup_writer: SubgroupWriter,
        mut now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        // Everything but the second.
        let base = now.format("%Y-%m-%d %H:%M:").to_string();

        subgroup_writer
            .write(base.clone().into())
            .context("failed to write base")?;

        loop {
            let delta = now.format("%S").to_string();
            subgroup_writer
                .write(delta.clone().into())
                .context("failed to write delta")?;

            println!("{base}{delta}");

            let next = now + chrono::Duration::try_seconds(1).unwrap();
            let next = next.with_nanosecond(0).unwrap();

            // Sleep until the next second
            let delay = (next - now).to_std().unwrap();
            tokio::time::sleep(delay).await;

            // Get the current time again to check if we overslept
            let next = Utc::now();
            if next.minute() != now.minute() {
                return Ok(());
            }

            now = next;
        }
    }
}

/// Subscribes to the clock and prints received time updates to stdout.
pub struct Subscriber {
    track_reader: TrackReader,
}

impl Subscriber {
    pub fn new(track_reader: TrackReader) -> Self {
        Self { track_reader }
    }

    /// Runs the subscriber, receiving time updates and printing them to stdout.
    pub async fn run(self) -> anyhow::Result<()> {
        match self
            .track_reader
            .mode()
            .await
            .context("failed to get mode")?
        {
            TrackReaderMode::Stream(stream) => Self::recv_stream(stream).await,
            TrackReaderMode::Subgroups(subgroups) => Self::recv_subgroups(subgroups).await,
            TrackReaderMode::Datagrams(datagrams) => Self::recv_datagrams(datagrams).await,
        }
    }

    /// Receives time updates from a stream and prints them to stdout.
    async fn recv_stream(mut stream_reader: StreamReader) -> anyhow::Result<()> {
        while let Some(mut stream_group_reader) = stream_reader.next().await? {
            while let Some(object) = stream_group_reader.read_next().await? {
                let str = String::from_utf8_lossy(&object);
                println!("{str}");
            }
        }

        Ok(())
    }

    /// Receives time updates from subgroups and prints them to stdout.
    async fn recv_subgroups(mut subgroups_reader: SubgroupsReader) -> anyhow::Result<()> {
        while let Some(mut subgroup_reader) = subgroups_reader.next().await? {
            // Spawn a new task to handle the subgroup concurrently, so we
            // don't rely on the publisher ending the previous stream before starting a new one.
            task::spawn(async move {
                if let Err(e) = async {
                    let base = subgroup_reader
                        .read_next()
                        .await
                        .context("failed to get first object")?
                        .context("empty subgroup")?;

                    let base = String::from_utf8_lossy(&base);

                    while let Some(object) = subgroup_reader.read_next().await? {
                        let str = String::from_utf8_lossy(&object);
                        println!("{base}{str}");
                    }

                    Ok::<(), anyhow::Error>(())
                }
                .await
                {
                    eprintln!("Error handling subgroup: {:?}", e);
                }
            });
        }

        Ok(())
    }

    /// Receives time updates from datagrams and prints them to stdout.
    async fn recv_datagrams(mut datagrams_reader: DatagramsReader) -> anyhow::Result<()> {
        while let Some(datagram) = datagrams_reader.read().await? {
            let str = String::from_utf8_lossy(&datagram.payload);
            println!("{str}");
        }

        Ok(())
    }
}
