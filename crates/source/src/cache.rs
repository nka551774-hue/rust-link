//! Bounded typed O(1) index-slab LRU cache.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustlink_protocol::{LoadResult, Track};

use crate::{MAX_CACHED_RESULT, MAX_CACHE_BYTES};

struct Entry {
    key: Arc<str>,
    data: Arc<LoadResult>,
    expires: Instant,
    bytes: usize,
    prev: Option<usize>,
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
        let entry = self.slots[index].as_ref()?;
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
        let reused = self.entries.remove(key).map(|idx| self.take_slot(idx).key);
        if limit == 0 || ttl.is_zero() || bytes > MAX_CACHED_RESULT {
            return;
        }
        let bytes = bytes + key.len() + size_of::<Entry>() + 4 * size_of::<usize>();
        while self.entries.len() >= limit || self.bytes + bytes > MAX_CACHE_BYTES {
            let Some(oldest) = self.first else { return };
            self.remove(oldest);
        }
        let index = self.vacant.pop().unwrap_or_else(|| {
            self.slots.push(None);
            self.slots.len() - 1
        });
        let key = reused.unwrap_or_else(|| Arc::from(key));
        self.entries.insert(Arc::clone(&key), index);
        self.slots[index] =
            Some(Entry { key, data, bytes, expires: Instant::now() + ttl, prev: None, next: None });
        self.bytes += bytes;
        self.append(index);
    }

    fn detach(&mut self, index: usize) {
        let e = self.slots[index].as_ref().unwrap();
        let (prev, next) = (e.prev, e.next);
        match prev {
            Some(p) => self.slots[p].as_mut().unwrap().next = next,
            None => self.first = next,
        }
        match next {
            Some(n) => self.slots[n].as_mut().unwrap().prev = prev,
            None => self.last = prev,
        }
    }

    fn append(&mut self, index: usize) {
        let prev = self.last.replace(index);
        let e = self.slots[index].as_mut().unwrap();
        e.prev = prev;
        e.next = None;
        match prev {
            Some(p) => self.slots[p].as_mut().unwrap().next = Some(index),
            None => self.first = Some(index),
        }
    }

    fn take_slot(&mut self, index: usize) -> Entry {
        self.detach(index);
        let entry = self.slots[index].take().unwrap();
        self.bytes -= entry.bytes;
        self.vacant.push(index);
        entry
    }

    fn remove(&mut self, index: usize) {
        let entry = self.take_slot(index);
        self.entries.remove(&entry.key);
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.slots.clear();
        self.vacant.clear();
        self.first = None;
        self.last = None;
        self.bytes = 0;
    }
}

pub(super) fn result_size(result: &LoadResult) -> Option<usize> {
    let heap = match result {
        LoadResult::Track(t) => size_of::<Track>().saturating_add(track_heap(t)),
        LoadResult::Playlist(p) => size_of_val(p.as_ref())
            .saturating_add(p.metadata.name.capacity())
            .saturating_add(tracks_heap(&p.tracks, p.tracks.capacity()))
            .saturating_add(object_heap(&p.extra)),
        LoadResult::Search(t) => tracks_heap(t, t.capacity()),
        LoadResult::Empty => 0,
        LoadResult::Error(_) => return None,
    };
    let bytes = size_of::<LoadResult>().saturating_add(heap).saturating_add(2 * size_of::<usize>());
    (bytes <= MAX_CACHED_RESULT).then_some(bytes)
}

fn tracks_heap(tracks: &[Track], cap: usize) -> usize {
    cap.saturating_mul(size_of::<Track>())
        .saturating_add(tracks.iter().map(track_heap).fold(0usize, usize::saturating_add))
}

fn track_heap(track: &Track) -> usize {
    let m = &track.metadata;
    let opt = |s: &Option<String>| s.as_ref().map_or(0, String::capacity);
    [
        track.token.capacity(),
        m.identifier.capacity(),
        m.title.capacity(),
        m.author.capacity(),
        m.source_name.capacity(),
        opt(&m.uri),
        opt(&m.artwork_url),
        opt(&m.isrc),
        object_heap(&track.extra),
        object_heap(&track.user_data),
    ]
    .into_iter()
    .fold(0usize, usize::saturating_add)
}

fn object_heap(object: &rustlink_protocol::Object) -> usize {
    const NODE: usize =
        size_of::<String>() + size_of::<serde_json::Value>() + size_of::<[usize; 8]>();
    object.iter().fold(0usize, |acc, (k, v)| {
        acc.saturating_add(k.capacity()).saturating_add(NODE).saturating_add(value_heap(v))
    })
}

fn value_heap(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => s.capacity(),
        serde_json::Value::Array(items) => items
            .capacity()
            .saturating_mul(size_of::<serde_json::Value>())
            .saturating_add(items.iter().map(value_heap).fold(0usize, usize::saturating_add)),
        serde_json::Value::Object(obj) => object_heap(obj),
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
        assert_eq!((cache.entries.len(), cache.slots.len()), (2, 2));
    }

    #[test]
    fn expiration_byte_budget_and_oversized_results_are_bounded() {
        let mut cache = Cache::default();
        for idx in 0..100 {
            put(&mut cache, &idx.to_string(), MAX_CACHED_RESULT, 100);
        }
        assert!(cache.bytes <= MAX_CACHE_BYTES && cache.entries.len() < 16);
        put(&mut cache, "huge", MAX_CACHED_RESULT + 1, 100);
        assert!(cache.get("huge").is_none());
        cache.put("expired", Arc::new(LoadResult::Empty), 8, Duration::ZERO, 100);
        assert!(cache.get("expired").is_none());
        let idx = cache.entries["99"];
        cache.slots[idx].as_mut().unwrap().expires = Instant::now();
        assert!(cache.get("99").is_none() && cache.get("98").is_some());
    }

    #[test]
    fn hits_share_storage_and_typed_capacity_counts_towards_the_budget() {
        let mut cache = Cache::default();
        let result = Arc::new(LoadResult::Empty);
        cache.put("a", result.clone(), result_size(&result).unwrap(), Duration::from_secs(60), 2);
        assert!(Arc::ptr_eq(&result, &cache.get("a").unwrap()));
        let over =
            LoadResult::Search(Vec::with_capacity(MAX_CACHED_RESULT / size_of::<Track>() + 1));
        assert!(result_size(&over).is_none());
    }

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
                let (bytes, exp) = self.entries.get(key)?;
                (*exp > Instant::now()).then(|| {
                    let out = bytes.clone();
                    self.order.retain(|k| k != key);
                    self.order.push_back(key.to_owned());
                    out
                })
            }
        }
        fn measure(iters: usize, mut run: impl FnMut(usize)) -> f64 {
            for i in 0..100 {
                run(i);
            }
            let start = Instant::now();
            for i in 0..iters {
                run(i);
            }
            start.elapsed().as_nanos() as f64 / iters as f64
        }
        for (entries, tracks, iters) in [(1024, 1, 50_000), (32, 100, 10_000)] {
            let result = Arc::new(LoadResult::Search(
                (0..tracks)
                    .map(|i| {
                        rustlink_protocol::encode_track(&TrackInfo {
                            identifier: format!("video{i:06}"),
                            title: format!("Fixture song {i}"),
                            author: "Fixture artist".into(),
                            length: 180_000,
                            source_name: "youtube".into(),
                            uri: Some(format!("https://example.com/watch?v={i}")),
                            ..Default::default()
                        })
                        .unwrap()
                    })
                    .collect(),
            ));
            let data = serde_json::to_vec(result.as_ref()).unwrap();
            let bytes = result_size(&result).unwrap();
            let keys: Vec<_> = (0..entries).map(|i| format!("fixture-{i}")).collect();
            let mut legacy = Legacy { entries: HashMap::new(), order: VecDeque::new() };
            let mut modern = Cache::default();
            let ttl = Duration::from_secs(300);
            for key in &keys {
                legacy.entries.insert(key.clone(), (data.clone(), Instant::now() + ttl));
                legacy.order.push_back(key.clone());
                modern.put(key, result.clone(), bytes, ttl, entries);
            }
            assert_eq!(modern.entries.len(), entries);
            let (legacy, modern) = (Mutex::new(legacy), Mutex::new(modern));
            let old = measure(iters, |i| {
                let raw = legacy.lock().unwrap().get(&keys[i % entries]).unwrap();
                black_box(serde_json::from_slice::<LoadResult>(&raw).unwrap());
            });
            let shared = measure(iters, |i| {
                black_box(modern.lock().unwrap().get(&keys[i % entries]).unwrap());
            });
            println!(
                "cache entries={entries} tracks={tracks} json_bytes={} ns/op: legacy={old:.1} shared={shared:.1}",
                data.len()
            );
        }
    }
}
