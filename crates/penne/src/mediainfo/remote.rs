//! Byte-range access backed by complete, CRC-checked yEnc articles.
use std::collections::HashMap;

use anyhow::{bail, ensure, Context, Result};
use pesto::yenc::DecodedPart;

use crate::{client::DownloadClient, config::Config, queue::QueuedFile};

pub(super) trait Source {
    async fn fetch(&mut self, file: &QueuedFile, index: usize) -> Result<DecodedPart>;
    fn stats(&self) -> (u64, usize);
}

pub(super) struct NntpSource {
    config: Config,
    clients: Vec<Option<DownloadClient>>,
    limit: u64,
    bytes: u64,
    articles: usize,
}

impl NntpSource {
    pub fn new(config: Config, limit: u64) -> Self {
        let count = config.server_tiers.iter().map(|t| t.members.len()).sum();
        Self {
            config,
            clients: (0..count).map(|_| None).collect(),
            limit,
            bytes: 0,
            articles: 0,
        }
    }

    pub async fn close(&mut self) {
        for client in &mut self.clients {
            if let Some(client) = client.take() {
                client.quit().await;
            }
        }
    }
}

impl Source for NntpSource {
    async fn fetch(&mut self, file: &QueuedFile, index: usize) -> Result<DecodedPart> {
        let segment = &file.segments[index];
        let mut last_error = None;
        for (server_index, server) in self
            .config
            .server_tiers
            .iter()
            .flat_map(|t| &t.members)
            .enumerate()
        {
            for attempt in 0..=self.config.retries {
                ensure!(
                    self.bytes < self.limit && segment.bytes <= self.limit - self.bytes,
                    "partial MediaInfo download budget exhausted ({} bytes); increase --max-bytes",
                    self.limit
                );
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(server.retry_delay)).await;
                }
                if self.clients[server_index].is_none() {
                    match DownloadClient::connect(server).await {
                        Ok(client) => self.clients[server_index] = Some(client),
                        Err(error) => {
                            last_error = Some(error);
                            continue;
                        }
                    }
                }
                let client = self.clients[server_index]
                    .as_mut()
                    .expect("connected above");
                let before = client.bytes_read();
                let body = client.body(&segment.message_id).await;
                // Include failed transfers and protocol bytes, not only successful payloads.
                self.bytes = self
                    .bytes
                    .saturating_add(client.bytes_read().saturating_sub(before));
                self.articles += 1;
                ensure!(self.bytes <= self.limit, "partial MediaInfo download budget exhausted ({} bytes); an article cannot be fetched partially", self.limit);
                match body {
                    Ok(Some(body)) => match pesto::yenc::decode_part(&body) {
                        Ok(part) if part.crc_matches() => return Ok(part),
                        Ok(_) => last_error = Some(anyhow::anyhow!("yEnc CRC mismatch")),
                        Err(error) => last_error = Some(error),
                    },
                    Ok(None) => {
                        last_error = Some(anyhow::anyhow!("article is missing"));
                        break;
                    }
                    Err(error) => {
                        self.clients[server_index] = None;
                        last_error = Some(error);
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no news servers configured")))
            .with_context(|| format!("fetching {} part {}", file.name, segment.part))
    }

    fn stats(&self) -> (u64, usize) {
        (self.bytes, self.articles)
    }
}

pub(super) struct Remote<S> {
    pub files: Vec<QueuedFile>,
    pub source: S,
    parts: HashMap<(usize, usize), DecodedPart>,
    sizes: HashMap<usize, u64>,
}

impl<S: Source> Remote<S> {
    pub fn new(files: Vec<QueuedFile>, source: S) -> Self {
        Self {
            files,
            source,
            parts: HashMap::new(),
            sizes: HashMap::new(),
        }
    }

    async fn part(&mut self, file: usize, index: usize) -> Result<&DecodedPart> {
        if !self.parts.contains_key(&(file, index)) {
            let part = self.source.fetch(&self.files[file], index).await?;
            ensure!(
                part.begin > 0
                    && part.end >= part.begin
                    && part.end <= part.file_size
                    && part.end - part.begin + 1 == part.data.len() as u64,
                "invalid yEnc byte range for {}",
                self.files[file].name
            );
            if let Some(size) = self.sizes.get(&file) {
                ensure!(*size == part.file_size, "inconsistent yEnc file sizes");
            }
            self.sizes.insert(file, part.file_size);
            self.parts.insert((file, index), part);
        }
        Ok(&self.parts[&(file, index)])
    }

    pub async fn size(&mut self, file: usize) -> Result<u64> {
        if let Some(size) = self.sizes.get(&file) {
            return Ok(*size);
        }
        ensure!(
            !self.files[file].segments.is_empty(),
            "file has no NZB segments"
        );
        Ok(self.part(file, 0).await?.file_size)
    }

    pub async fn read(&mut self, file: usize, offset: u64, length: usize) -> Result<Vec<u8>> {
        let size = self.size(file).await?;
        ensure!(
            offset <= size && length as u64 <= size - offset,
            "range outside file"
        );
        let mut output = Vec::with_capacity(length);
        let mut position = offset;
        while output.len() < length {
            // yEnc offsets are authoritative. NZB byte counts are encoded sizes,
            // so they must never be used as decoded offsets. Binary search also
            // handles variable-sized articles and missing NZB part numbers.
            let cached = self.parts.iter().find_map(|(&(f, i), p)| {
                (f == file && p.begin - 1 <= position && position < p.end).then_some(i)
            });
            let index = if let Some(index) = cached {
                index
            } else {
                let count = self.files[file].segments.len();
                let first_length = self
                    .parts
                    .get(&(file, 0))
                    .map(|p| p.data.len() as u64)
                    .unwrap_or(1)
                    .max(1);
                let guess = (position / first_length).min((count - 1) as u64) as usize;
                let guessed = self.part(file, guess).await?;
                let (mut low, mut high, mut found) = if position < guessed.begin - 1 {
                    (0, guess, None)
                } else if position >= guessed.end {
                    (guess + 1, count, None)
                } else {
                    (0, 0, Some(guess))
                };
                while low < high {
                    let mid = low + (high - low) / 2;
                    let p = self.part(file, mid).await?;
                    if position < p.begin - 1 {
                        high = mid;
                    } else if position >= p.end {
                        low = mid + 1;
                    } else {
                        found = Some(mid);
                        break;
                    }
                }
                match found {
                    Some(index) => index,
                    None => bail!("missing NZB segment covering byte {position}"),
                }
            };
            let p = self.part(file, index).await?;
            let start = (position - (p.begin - 1)) as usize;
            let count = (length - output.len()).min(p.data.len() - start);
            output.extend_from_slice(&p.data[start..start + count]);
            position += count as u64;
        }
        Ok(output)
    }
}
