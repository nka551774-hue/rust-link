//! A bounded, typed LRU. Hits only clone an Arc and unlink/relink two indices.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustlink_protocol::{LoadResult, Track};

use crate::{MAX_CACHED_RESULT, MAX_CACHE_BYTES};

struct Entry {
    key: Arc<str>,
    data: Arc<LoadResult>,
    expires: Instant,
    bytes: usize,
    previous: Option<usize>,
    next: Option<usize>,
}

#[derive(Default)]
pub(super) struct Cache {
    entries: HashMap<Arc<str>, usize>,
    slots: Vec<Option<Entry>>,
    vacant: Vec<usize>,
    first: Option<usize>,
    last: Option<usize>,
    bytes: usize,
}

impl Cache {
    pub(super) fn get(&mut self, key: &str) -> Option<Arc<LoadResult>> {
        let index = *self.entries.get(key)?;
        let entry = self.slots[index].as_ref().expect("indexed entry");
        if entry.expires <= Instant::now() {
            self.remove(index);
            return None;
        }
        let data = Arc::clone(&entry.data);
        if self.last != Some(index) {
            self.detach(index);
            self.append(index);
        }
        Some(data)
    }

    pub(super) fn put(
        &mut self,
        key: &str,
        data: Arc<LoadResult>,
        bytes: usize,
        ttl: Duration,
        limit: usize,
    ) {
        if let Some(&index) = self.entries.get(key) {
            self.remove(index);
        }
        if limit == 0 || ttl.is_zero() || bytes > MAX_CACHED_RESULT {
            return;
        }
        // Include the key and list/map bookkeeping in the retained byte budget.
        let bytes = bytes + key.len() + size_of::<Entry>() + 4 * size_of::<usize>();
        while self.entries.len() >= limit || self.bytes + bytes > MAX_CACHE_BYTES {
            let Some(index) = self.first else { return };
            self.remove(index);
        }
        let index = self.vacant.pop().unwrap_or_else(|| {
            self.slots.push(None);
            self.slots.len() - 1
        });
        let key: Arc<str> = Arc::from(key);
        self.entries.insert(Arc::clone(&key), index);
        self.slots[index] = Some(Entry {
            key,
            data,
            bytes,
            expires: Instant::now() + ttl,
            previous: None,
            next: None,
        });
        self.bytes += bytes;
        self.append(index);
    }

    fn detach(&mut self, index: usize) {
        let entry = self.slots[index].as_ref().expect("indexed entry");
        let (previous, next) = (entry.previous, entry.next);
        if let Some(previous) = previous {
            self.slots[previous].as_mut().expect("linked entry").next = next;
        } else {
            self.first = next;
        }
        if let Some(next) = next {
            self.slots[next].as_mut().expect("linked entry").previous = previous;
        } else {
            self.last = previous;
        }
    }

    fn append(&mut self, index: usize) {
        let entry = self.slots[index].as_mut().expect("indexed entry");
        entry.previous = self.last;
        entry.next = None;
        if let Some(last) = self.last {
            self.slots[last].as_mut().expect("linked entry").next = Some(index);
        } else {
            self.first = Some(index);
        }
        self.last = Some(index);
    }

    fn remove(&mut self, index: usize) {
        self.detach(index);
        let entry = self.slots[index].take().expect("indexed entry");
        self.entries.remove(&entry.key);
        self.bytes -= entry.bytes;
        self.vacant.push(index);
    }
}

/// Count encoded bytes without allocating a temporary JSON buffer. Also charge
/// the typed representation's containers, capacities, and strings so switching
/// away from JSON storage cannot silently defeat the memory bound.
pub(super) fn result_size(result: &LoadResult) -> Option<usize> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > MAX_CACHED_RESULT {
                return Err(io::Error::other("result exceeds cache entry budget"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, result).ok()?;
    let heap = match result {
        LoadResult::Track(track) => size_of::<Track>() + track_heap(track),
        LoadResult::Playlist(playlist) => {
            size_of_val(playlist.as_ref())
                + playlist.metadata.name.capacity()
                + playlist.tracks.capacity() * size_of::<Track>()
                + playlist.tracks.iter().map(track_heap).sum::<usize>()
                + object_heap(&playlist.extra)
        }
        LoadResult::Search(tracks) => {
            tracks.capacity() * size_of::<Track>() + tracks.iter().map(track_heap).sum::<usize>()
        }
        LoadResult::Empty => 0,
        LoadResult::Error(_) => return None,
    };
    let bytes = counter.0.max(size_of::<LoadResult>() + heap + 2 * size_of::<usize>());
    (bytes <= MAX_CACHED_RESULT).then_some(bytes)
}

fn track_heap(track: &Track) -> usize {
    let metadata = &track.metadata;
    track.token.capacity()
        + metadata.identifier.capacity()
        + metadata.title.capacity()
        + metadata.author.capacity()
        + metadata.source_name.capacity()
        + metadata.uri.as_ref().map_or(0, String::capacity)
        + metadata.artwork_url.as_ref().map_or(0, String::capacity)
        + metadata.isrc.as_ref().map_or(0, String::capacity)
        + object_heap(&track.extra)
        + object_heap(&track.user_data)
}

fn object_heap(object: &rustlink_protocol::Object) -> usize {
    // A conservative per-entry allowance covers BTreeMap node occupancy and
    // pointers as well as the inline key/value. No JSON object is cached free.
    object
        .iter()
        .map(|(key, value)| {
            key.capacity()
                + size_of::<String>()
                + size_of::<serde_json::Value>()
                + size_of::<[usize; 8]>()
                + value_heap(value)
        })
        .sum()
}

fn value_heap(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(text) => text.capacity(),
        serde_json::Value::Array(items) => {
            items.capacity() * size_of::<serde_json::Value>()
                + items.iter().map(value_heap).sum::<usize>()
        }
        serde_json::Value::Object(object) => object_heap(object),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(cache: &mut Cache, key: &str, bytes: usize, limit: usize) {
        cache.put(key, Arc::new(LoadResult::Empty), bytes, Duration::from_secs(60), limit);
    }

    #[test]
    fn access_promotes_and_replacement_preserves_links() {
        let mut cache = Cache::default();
        put(&mut cache, "a", 8, 2);
        put(&mut cache, "b", 8, 2);
        assert!(cache.get("a").is_some());
        put(&mut cache, "c", 8, 2);
        assert!(cache.get("b").is_none());
        put(&mut cache, "a", 16, 2);
        put(&mut cache, "d", 8, 2);
        assert!(cache.get("c").is_none());
        assert!(cache.get("a").is_some());
        assert!(cache.get("d").is_some());
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.slots.len(), 2);
    }

    #[test]
    fn expiration_byte_budget_and_oversized_results_are_bounded() {
        let mut cache = Cache::default();
        for index in 0..100 {
            put(&mut cache, &index.to_string(), MAX_CACHED_RESULT, 100);
        }
        assert!(cache.bytes <= MAX_CACHE_BYTES);
        assert!(cache.entries.len() < 16);
        put(&mut cache, "huge", MAX_CACHED_RESULT + 1, 100);
        assert!(cache.get("huge").is_none());
        cache.put("expired", Arc::new(LoadResult::Empty), 8, Duration::ZERO, 100);
        assert!(cache.get("expired").is_none());
        let index = *cache.entries.get("99").unwrap();
        cache.slots[index].as_mut().unwrap().expires = Instant::now();
        assert!(cache.get("99").is_none());
        assert!(cache.get("98").is_some());
    }

    #[test]
    fn hits_share_storage_and_typed_capacity_counts_towards_the_budget() {
        let mut cache = Cache::default();
        let result = Arc::new(LoadResult::Empty);
        cache.put("a", result.clone(), result_size(&result).unwrap(), Duration::from_secs(60), 2);
        assert!(Arc::ptr_eq(&result, &cache.get("a").unwrap()));
        let result =
            LoadResult::Search(Vec::with_capacity(MAX_CACHED_RESULT / size_of::<Track>() + 1));
        assert!(result_size(&result).is_none());
    }

    /// Reproduce the previous byte-cache + VecDeque::retain + JSON decode path
    /// in the same optimized process. No network, timing threshold or claim
    /// about end-to-end playback performance is hidden in this microbenchmark.
    #[test]
    #[ignore = "release microbenchmark; run with --release --ignored --nocapture"]
    fn benchmark_resolution_cache() {
        use rustlink_protocol::TrackInfo;
        use std::collections::VecDeque;
        use std::hint::black_box;
        use std::sync::Mutex;

        struct Legacy {
            entries: HashMap<String, (Vec<u8>, Instant)>,
            order: VecDeque<String>,
        }
        impl Legacy {
            fn get(&mut self, key: &str) -> Option<Vec<u8>> {
                let entry = self.entries.get(key)?;
                if entry.1 <= Instant::now() {
                    return None;
                }
                let data = entry.0.clone();
                self.order.retain(|held| held != key);
                self.order.push_back(key.to_owned());
                Some(data)
            }
        }

        fn measure(iterations: usize, mut run: impl FnMut(usize)) -> f64 {
            for index in 0..100 {
                run(index);
            }
            let start = Instant::now();
            for index in 0..iterations {
                run(index);
            }
            start.elapsed().as_nanos() as f64 / iterations as f64
        }

        for (entries, tracks, iterations) in [(1024, 1, 50_000), (32, 100, 10_000)] {
            let result = Arc::new(LoadResult::Search(
                (0..tracks)
                    .map(|index| {
                        rustlink_protocol::encode_track(&TrackInfo {
                            identifier: format!("video{index:06}"),
                            title: format!("Fixture song {index}"),
                            author: "Fixture artist".into(),
                            length: 180_000,
                            source_name: "youtube".into(),
                            uri: Some(format!("https://example.com/watch?v={index}")),
                            ..Default::default()
                        })
                        .unwrap()
                    })
                    .collect(),
            ));
            let data = serde_json::to_vec(result.as_ref()).unwrap();
            let bytes = result_size(&result).unwrap();
            let keys: Vec<_> = (0..entries).map(|index| format!("fixture-{index}")).collect();
            let mut legacy = Legacy { entries: HashMap::new(), order: VecDeque::new() };
            let mut modern = Cache::default();
            for key in &keys {
                legacy
                    .entries
                    .insert(key.clone(), (data.clone(), Instant::now() + Duration::from_secs(300)));
                legacy.order.push_back(key.clone());
                modern.put(key, result.clone(), bytes, Duration::from_secs(300), entries);
            }
            assert_eq!(modern.entries.len(), entries);
            let legacy = Mutex::new(legacy);
            let modern = Mutex::new(modern);
            let mut timings = Vec::new();
            for _ in 0..3 {
                let old = measure(iterations, |index| {
                    let bytes = legacy.lock().unwrap().get(&keys[index % entries]).unwrap();
                    black_box(serde_json::from_slice::<LoadResult>(&bytes).unwrap());
                });
                let owned = measure(iterations, |index| {
                    let result = modern.lock().unwrap().get(&keys[index % entries]).unwrap();
                    black_box(result.as_ref().clone());
                });
                let shared = measure(iterations, |index| {
                    black_box(modern.lock().unwrap().get(&keys[index % entries]).unwrap());
                });
                timings.push([old, owned, shared]);
            }
            let mut medians = [0.0; 3];
            for column in 0..3 {
                let mut samples = timings.iter().map(|row| row[column]).collect::<Vec<_>>();
                samples.sort_by(f64::total_cmp);
                medians[column] = samples[1];
            }
            println!("cache entries={entries} tracks={tracks} json_bytes={} iterations={iterations} median_ns/op: legacy={:.1} typed_owned={:.1} typed_shared={:.1}",
                data.len(), medians[0], medians[1], medians[2]);
        }
    }
}
