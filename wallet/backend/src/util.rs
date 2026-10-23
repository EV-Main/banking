/// Current unix time in seconds (matches the proto `*_expires_at` / `*_at` fields).
pub fn now_secs() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
