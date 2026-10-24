//! Training a compression dictionary for each kind of value.

use crate::compress::Dictionary;
use crate::Kind;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

/// How many samples to collect before training, at first.
const SAMPLES_TO_TRAIN: usize = 1000;

/// Train early once the samples reach this size.
const SAMPLE_BYTES_TO_TRAIN: usize = 1 << 20;

/// Give up on a kind after this many failed attempts, each with twice the
/// samples of the last.
const MAX_ATTEMPTS: u32 = 4;

/// The most sample bytes held at once, across every kind, so that kinds
/// still collecting samples don't add up to much.
const MAX_TOTAL_SAMPLE_BYTES: usize = 8 << 20;

/// The smallest and largest dictionaries trained.
const MIN_DICTIONARY_LEN: usize = 1024;
const MAX_DICTIONARY_LEN: usize = 32 * 1024;

/// The dictionary, if any, for each kind of value, and the samples being
/// collected for kinds that don't have one yet.
#[derive(Default)]
pub(crate) struct Dictionaries {
    dictionaries: RwLock<Vec<Option<Arc<Dictionary>>>>,
    training: Mutex<Vec<Training>>,
}

#[derive(Default)]
struct Training {
    samples: Vec<Vec<u8>>,
    sample_bytes: usize,
    attempts: u32,
    in_progress: bool,
}

impl Training {
    fn wants_samples(&self) -> bool {
        self.attempts < MAX_ATTEMPTS && !self.in_progress
    }

    fn ready(&self) -> bool {
        let scale = 1 << self.attempts;
        self.samples.len() >= SAMPLES_TO_TRAIN * scale || self.sample_bytes >= SAMPLE_BYTES_TO_TRAIN * scale
    }
}

impl Dictionaries {
    pub(crate) fn get(&self, kind: Kind) -> Option<Arc<Dictionary>> {
        let dictionaries = self.dictionaries.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        dictionaries.get(kind.0 as usize).cloned().flatten()
    }

    /// Record a value of a kind that has no dictionary yet. Returns the
    /// samples to train on once there are enough; the caller trains outside
    /// any lock with [`Dictionaries::train`].
    pub(crate) fn add_sample(&self, kind: Kind, bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
        let mut training = self.lock_training();
        let index = kind.0 as usize;
        if training.len() <= index {
            training.resize_with(index + 1, Training::default);
        }
        let total: usize = training.iter().map(|state| state.sample_bytes).sum();
        let state = &mut training[index];
        if !state.wants_samples() || total + bytes.len() > MAX_TOTAL_SAMPLE_BYTES {
            return None;
        }
        state.samples.push(bytes.to_vec());
        state.sample_bytes += bytes.len();
        if !state.ready() {
            return None;
        }
        state.in_progress = true;
        state.sample_bytes = 0;
        Some(std::mem::take(&mut state.samples))
    }

    /// Train a dictionary from `samples`, install it if training succeeds,
    /// and return it.
    pub(crate) fn train(&self, kind: Kind, samples: &[Vec<u8>]) -> Option<Arc<Dictionary>> {
        let total: usize = samples.iter().map(Vec::len).sum();
        let max_len = (total / 32).clamp(MIN_DICTIONARY_LEN, MAX_DICTIONARY_LEN);
        let dictionary = Dictionary::train(samples, max_len).map(Arc::new);

        let mut training = self.lock_training();
        let state = &mut training[kind.0 as usize];
        state.in_progress = false;
        let Some(dictionary) = dictionary else {
            state.attempts += 1;
            return None;
        };
        state.attempts = MAX_ATTEMPTS;
        drop(training);

        let mut dictionaries = self.dictionaries.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = kind.0 as usize;
        if dictionaries.len() <= index {
            dictionaries.resize(index + 1, None);
        }
        dictionaries[index] = Some(dictionary.clone());
        Some(dictionary)
    }

    /// The size of the dictionary for `kind`, or zero if it has none.
    pub(crate) fn bytes(&self, kind: Kind) -> usize {
        self.get(kind).map_or(0, |dictionary| dictionary.len())
    }

    fn lock_training(&self) -> MutexGuard<'_, Vec<Training>> {
        self.training.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
