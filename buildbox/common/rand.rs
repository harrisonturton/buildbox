use rand::distr::Alphanumeric;
use rand::Rng;

/// Get a random string of length.
pub fn string(len: usize) -> String {
    let data: Vec<u8> = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .collect();
    data_encoding::HEXLOWER.encode(&data)
}
