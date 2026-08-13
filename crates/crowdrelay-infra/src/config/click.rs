//! Click analytics buffer configuration parsing.

use super::*;

pub(super) fn parse_click_buffer_config(
    values: &HashMap<String, String>,
) -> Result<ClickBufferConfig, ConfigError> {
    let capacity = parse_bounded_u32(
        values.get(CLICK_CHANNEL_CAPACITY_KEY),
        CLICK_CHANNEL_CAPACITY_KEY,
        DEFAULT_CLICK_CHANNEL_CAPACITY,
        1,
        MAX_CLICK_CHANNEL_CAPACITY,
    )?;
    let batch_size = parse_bounded_u32(
        values.get(CLICK_BATCH_SIZE_KEY),
        CLICK_BATCH_SIZE_KEY,
        DEFAULT_CLICK_BATCH_SIZE,
        1,
        MAX_CLICK_BATCH_SIZE,
    )?;
    if batch_size > capacity {
        return Err(ConfigError::BatchExceedsCapacity {
            batch_name: CLICK_BATCH_SIZE_KEY,
            capacity_name: CLICK_CHANNEL_CAPACITY_KEY,
        });
    }
    let flush_interval = parse_bounded_duration(
        values.get(CLICK_FLUSH_INTERVAL_MS_KEY),
        CLICK_FLUSH_INTERVAL_MS_KEY,
        DEFAULT_CLICK_FLUSH_INTERVAL_MS,
        MIN_CLICK_FLUSH_INTERVAL_MS,
        MAX_CLICK_FLUSH_INTERVAL_MS,
    )?;

    Ok(ClickBufferConfig {
        capacity: capacity as usize,
        batch_size: batch_size as usize,
        flush_interval,
    })
}
