pub mod output;
pub mod source;
pub mod stage;
pub mod theme;
pub mod transport;

use gstreamer as gst;

/// `mm:ss.d` or `h:mm:ss.d`.
pub fn format_clock(time: gst::ClockTime) -> String {
    let total_tenths = time.nseconds() / 100_000_000;
    let hours = total_tenths / 36_000;
    let minutes = (total_tenths / 600) % 60;
    let seconds = (total_tenths / 10) % 60;
    let tenths = total_tenths % 10;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}.{tenths}")
    } else {
        format!("{minutes:02}:{seconds:02}.{tenths}")
    }
}
