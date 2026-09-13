pub mod didl;
pub mod http;
pub mod ids;
pub mod remux;
pub mod server;
pub mod ssdp;
#[cfg(test)]
pub(crate) mod test_helpers;

// Today's public surface, unchanged: every `crate::dlna::<name>` path used by
// commands.rs, lib.rs and examples resolves here.
pub use didl::{ResKind, cover_art_uri, didl_for_episode};
pub use ids::{decode_id, encode_id, xml_escape};
pub use remux::{
    REMUX_CACHE_CAP_BYTES, REMUX_TIMEOUT, evict_remux_cache, ffmpeg_available, remux_cache_dir,
    remux_path, should_mark_played,
};
pub use server::{DlnaServer, browse, browse_with_opts, dlna_warning, set_dlna_error_sink};
pub use ssdp::ssdp_msearch_reply;

// Test-only paths: the per-module `#[cfg(test)]` suites kept their
// `crate::dlna::<name>` references verbatim, which resolve through these.
// Gated so non-test builds see no unused re-exports.
#[cfg(test)]
pub(crate) use didl::{cover_file_name, mime_for, serve_cover_file};
#[cfg(test)]
pub(crate) use http::{Body, parse_range, serve_bytes_response, serve_file_response};
#[cfg(test)]
pub(crate) use remux::{
    build_ffmpeg_cmd, ensure_remux, ensure_remux_async, escape_filter_path, has_ass_subtitles,
    remux_seats, remux_tmp_path, sweep_stale_tmps,
};
#[cfg(test)]
pub(crate) use server::{
    Backend, ServeCtx, note_bytes_sent, serve_browse, serve_media, serve_remux, serve_remux_head,
};
#[cfg(test)]
pub(crate) use ssdp::{notify_alive, notify_byebye};
