use core::fmt::Display;
use std::{io::Write, num::NonZeroU32};

use crossterm::{
	cursor::MoveTo,
	event::EventStream,
	execute,
	terminal::{disable_raw_mode, enable_raw_mode}
};
use image::DynamicImage;
use kittage::{
	AsyncInputReader, ImageDimensions, ImageId, NumberOrId, PixelFormat, Verbosity,
	action::Action,
	delete::{ClearOrDelete, DeleteConfig, WhichToDelete},
	display::{CursorMovementPolicy, DisplayConfig, DisplayLocation},
	error::TransmitError,
	image::Image,
	medium::Medium
};
use ratatui::layout::Position;
use smallvec::SmallVec;

use crate::converter::MaybeTransferred;

pub struct KittyReadyToDisplay<'tui> {
	pub img: &'tui mut MaybeTransferred,
	pub page_num: usize,
	pub pos: Position,
	pub display_loc: DisplayLocation
}

pub enum KittyDisplay<'tui> {
	NoChange,
	ClearImages,
	DisplayImages(Vec<KittyReadyToDisplay<'tui>>)
}

pub struct DbgWriter<W: Write> {
	w: W,
	#[cfg(debug_assertions)]
	buf: String
}

impl<W: Write> Write for DbgWriter<W> {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		#[cfg(debug_assertions)]
		{
			if let Ok(s) = std::str::from_utf8(buf) {
				self.buf.push_str(s);
			}
		}
		self.w.write(buf)
	}

	fn flush(&mut self) -> std::io::Result<()> {
		#[cfg(debug_assertions)]
		{
			log::debug!("Writing to kitty: {:?}", self.buf);
			self.buf.clear();
		}
		self.w.flush()
	}
}

struct SilentHeaderPatcher<W> {
	inner: W,
	buf: [u8; 7],
	len: usize,
	done: bool
}

impl<W> SilentHeaderPatcher<W> {
	fn new(inner: W) -> Self {
		Self {
			inner,
			buf: [0; 7],
			len: 0,
			done: false
		}
	}
}

impl<W: Write> Write for SilentHeaderPatcher<W> {
	fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
		if self.done {
			return self.inner.write(input);
		}
		if input.is_empty() {
			return Ok(0);
		}

		let take = (7 - self.len).min(input.len());
		self.buf[self.len..self.len + take].copy_from_slice(&input[..take]);
		self.len += take;

		if self.len < 7 {
			return Ok(take);
		}

		self.done = true;
		self.inner.write_all(&self.buf)?;
		// kittage's Action::write_transmit_to ignores Verbosity::Silent for Display (a=p) and
		// Delete (a=d), so inject q=2 right after their 7-byte header when suppressing replies.
		if &self.buf == b"\x1b_Ga=p," || &self.buf == b"\x1b_Ga=d," {
			self.inner.write_all(b"q=2,")?;
		}
		if take < input.len() {
			self.inner.write_all(&input[take..])?;
		}
		Ok(input.len())
	}

	fn flush(&mut self) -> std::io::Result<()> {
		if !self.done && self.len > 0 {
			self.done = true;
			self.inner.write_all(&self.buf[..self.len])?;
			self.len = 0;
		}
		self.inner.flush()
	}
}

pub fn is_tmux() -> bool {
	static IS_TMUX: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
	*IS_TMUX.get_or_init(|| {
		std::env::var_os("TMUX").is_some()
			|| std::env::var("TERM_PROGRAM").is_ok_and(|p| p == "tmux")
			|| std::env::var("TERM").is_ok_and(|t| t.starts_with("tmux"))
	})
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxPaneState {
	Focused,
	VisibleUnfocused,
	Hidden
}

#[must_use]
pub fn parse_tmux_pane_state(text: &str) -> TmuxPaneState {
	let mut cols = text.split_whitespace();
	let attached = cols.next() == Some("1");
	let win_active = cols.next() == Some("1");
	let pane_active = cols.next() == Some("1");
	let zoomed = cols.next() == Some("1");
	let in_mode = cols.next() == Some("1");

	if !attached || !win_active || in_mode || (zoomed && !pane_active) {
		TmuxPaneState::Hidden
	} else if pane_active {
		TmuxPaneState::Focused
	} else {
		TmuxPaneState::VisibleUnfocused
	}
}

#[must_use]
pub fn parse_tmux_cell_size(text: &str) -> Option<(u16, u16)> {
	let mut cols = text.split_whitespace();
	let w = cols.next()?.parse::<u16>().ok().filter(|&v| v > 0)?;
	let h = cols.next()?.parse::<u16>().ok().filter(|&v| v > 0)?;
	Some((w, h))
}

fn tmux_display_message(format: &str) -> Option<String> {
	if !is_tmux() {
		return None;
	}
	let mut cmd = std::process::Command::new("tmux");
	cmd.args(["display-message", "-p"]);
	if let Some(pane) = std::env::var_os("TMUX_PANE").filter(|p| !p.is_empty()) {
		cmd.arg("-t").arg(pane);
	}
	let out = cmd.arg(format).output().ok()?;
	if !out.status.success() {
		return None;
	}
	std::str::from_utf8(&out.stdout).ok().map(str::to_owned)
}

#[must_use]
pub fn tmux_client_cell_size() -> Option<(u16, u16)> {
	let text = tmux_display_message("#{client_cell_width} #{client_cell_height}")?;
	parse_tmux_cell_size(&text)
}

/// Overrides any pane-local `allow-passthrough on` (e.g. set by `ratatui_image::Picker`) with
/// `all` so `tdf` can still clear Kitty placements after switching to another tmux window.
pub fn ensure_tmux_passthrough_all() {
	if !is_tmux() {
		return;
	}
	let mut cmd = std::process::Command::new("tmux");
	cmd.args(["set-option", "-p"]);
	if let Some(pane) = std::env::var_os("TMUX_PANE").filter(|p| !p.is_empty()) {
		cmd.arg("-t").arg(pane);
	}
	let _ = cmd
		.args(["allow-passthrough", "all"])
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.status();
}

#[must_use]
pub fn tmux_pane_state() -> TmuxPaneState {
	if !is_tmux() {
		return TmuxPaneState::Focused;
	}
	if std::env::var_os("TMUX_PANE").is_none_or(|p| p.is_empty()) {
		return TmuxPaneState::Hidden;
	}
	let Some(text) = tmux_display_message(
		"#{session_attached} #{window_active} #{pane_active} #{window_zoomed_flag} #{pane_in_mode}"
	) else {
		return TmuxPaneState::Hidden;
	};
	parse_tmux_pane_state(&text)
}

fn is_tmux_pane_focused() -> bool {
	tmux_pane_state() == TmuxPaneState::Focused
}

pub fn clear_kitty_placements_silent() {
	let action = Action::Delete(DeleteConfig {
		effect: ClearOrDelete::Clear,
		which: WhichToDelete::All
	});
	let _ = write_silent_action::<std::io::Error>(&action);
}

fn transmit_silent<W: Write, E: std::error::Error>(
	action: &Action<'_, '_>,
	inner: W
) -> Result<(), TransmitError<E>> {
	let writer = SilentHeaderPatcher::new(DbgWriter {
		w: inner,
		#[cfg(debug_assertions)]
		buf: String::new()
	});
	action
		.write_transmit_to(writer, Verbosity::Silent)
		.map(|_| ())
		.map_err(TransmitError::Writing)
}

fn write_silent_action<E: std::error::Error>(
	action: &Action<'_, '_>
) -> Result<(), TransmitError<E>> {
	let stdout = std::io::stdout().lock();
	if is_tmux() {
		transmit_silent(action, kittage::tmux::TmuxWriter::new(stdout))
	} else {
		transmit_silent(action, stdout)
	}
}

fn run_silent_action<E: std::error::Error>(
	action: Action<'static, 'static>
) -> Result<(), TransmitError<E>> {
	write_silent_action(&action)?;
	// Delay dropping SHM-backed images because kittage unlinks /dev/shm objects in Drop, and when
	// writing with q=2 we don't wait for the terminal's OK response before returning.
	if matches!(
		action,
		Action::TransmitAndDisplay {
			image: Image {
				medium: Medium::SharedMemObject(_),
				..
			},
			..
		} | Action::Transmit(Image {
			medium: Medium::SharedMemObject(_),
			..
		})
	) {
		tokio::spawn(async move {
			tokio::time::sleep(std::time::Duration::from_millis(500)).await;
			drop(action);
		});
	}
	Ok(())
}

pub async fn run_action<'es>(
	action: Action<'_, '_>,
	ev_stream: &'es mut EventStream
) -> Result<Option<ImageId>, TransmitError<<&'es mut EventStream as AsyncInputReader>::Error>> {
	if is_tmux() && matches!(action, Action::Delete(_)) {
		write_silent_action(&action)?;
		return Ok(None);
	}

	let stdout = std::io::stdout().lock();
	if is_tmux() {
		let writer = DbgWriter {
			w: kittage::tmux::TmuxWriter::new(stdout),
			#[cfg(debug_assertions)]
			buf: String::new()
		};
		action
			.execute_async(writer, ev_stream)
			.await
			.map(|(_, i)| i)
	} else {
		let writer = DbgWriter {
			w: stdout,
			#[cfg(debug_assertions)]
			buf: String::new()
		};
		action
			.execute_async(writer, ev_stream)
			.await
			.map(|(_, i)| i)
	}
}

pub async fn do_shms_work(ev_stream: &mut EventStream) -> bool {
	if is_tmux() && !is_tmux_pane_focused() {
		return false;
	}

	let img = DynamicImage::new_rgb8(1, 1);
	let pid = std::process::id();
	let shm_name = format!("tdf_test_{pid}");

	#[cfg(unix)]
	let shm_name = &*shm_name;

	let Ok(mut k_img) = kittage::image::Image::shm_from(img, shm_name) else {
		return false;
	};

	// apparently the terminal won't respond to queries unless they have an Id instead of a number
	k_img.num_or_id = NumberOrId::Id(NonZeroU32::new(u32::MAX).unwrap());

	if enable_raw_mode().is_err() {
		return false;
	}

	let res = run_action(Action::Query(&k_img), ev_stream).await;

	let _ = disable_raw_mode();

	res.is_ok()
}

type ESTransErr<'es> = TransmitError<<&'es mut EventStream as AsyncInputReader>::Error>;

pub struct DisplayErr<'es> {
	pub failed_pages: SmallVec<[usize; 2]>,
	pub user_facing_err: &'static str,
	pub source: DisplayErrSource<'es>
}

impl<'es> DisplayErr<'es> {
	fn empty(user_facing_err: &'static str, source: ESTransErr<'es>) -> Self {
		Self {
			failed_pages: SmallVec::new(),
			user_facing_err,
			source: DisplayErrSource::Transmission(source)
		}
	}
}

#[derive(Debug)]
pub enum DisplayErrSource<'es> {
	KittageReturnedNoId,
	Transmission(ESTransErr<'es>)
}

impl Display for DisplayErrSource<'_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::KittageReturnedNoId => write!(
				f,
				"Kittage returned no ID when we asked it to display an image. This is a bug in kittage, please report it."
			),
			Self::Transmission(t) => write!(f, "Error with talking to the terminal: {t}")
		}
	}
}

pub async fn display_kitty_images<'es>(
	display: KittyDisplay<'_>,
	ev_stream: &'es mut EventStream,
	last_z_index: &mut i32
) -> Result<(), DisplayErr<'es>> {
	let images = match display {
		KittyDisplay::NoChange => return Ok(()),
		KittyDisplay::ClearImages =>
			return run_action(
				Action::Delete(DeleteConfig {
					effect: ClearOrDelete::Clear,
					which: WhichToDelete::All
				}),
				ev_stream
			)
			.await
			.map_err(|e| DisplayErr::empty("Couldn't clear previous images", e))
			.map(|_: Option<ImageId>| ()),
		KittyDisplay::DisplayImages(imgs) => imgs
	};

	let new_z_index = last_z_index.wrapping_add_unsigned(1);
	let mut tmux_focused: Option<bool> = None;

	let mut err = Ok::<(), (SmallVec<[usize; 2]>, DisplayErrSource<'es>)>(());
	for KittyReadyToDisplay {
		img,
		page_num,
		pos,
		mut display_loc
	} in images
	{
		display_loc.z_index = new_z_index;

		let config = DisplayConfig {
			location: display_loc,
			cursor_movement: CursorMovementPolicy::DontMove,
			..DisplayConfig::default()
		};

		if is_tmux() {
			let mut out = std::io::stdout().lock();
			if let Err(e) = execute!(out, MoveTo(pos.x, pos.y))
				.and_then(|()| out.write_all(b"\x1b[0m\x1b[X"))
				.and_then(|()| out.flush())
			{
				return Err(DisplayErr::empty(
					"Couldn't position cursor for image",
					TransmitError::Writing(e)
				));
			}
		} else if let Err(e) = execute!(std::io::stdout(), MoveTo(pos.x, pos.y)) {
			return Err(DisplayErr::empty(
				"Couldn't position cursor for image",
				TransmitError::Writing(e)
			));
		}

		log::debug!("going to display img {img:#?}");
		log::debug!("displaying with config {config:#?}");

		let this_err = match img {
			MaybeTransferred::NotYet(image) => {
				let mut fake_image = Image {
					num_or_id: image.num_or_id,
					format: PixelFormat::Rgb24(
						ImageDimensions {
							width: 0,
							height: 0
						},
						None
					),
					medium: Medium::Direct {
						chunk_size: None,
						data: (&[]).into()
					}
				};
				std::mem::swap(image, &mut fake_image);

				let swapped_id = match fake_image.num_or_id {
					NumberOrId::Id(id) => Some(id),
					NumberOrId::Number(_) => None
				};
				let action = Action::TransmitAndDisplay {
					image: fake_image,
					config,
					placement_id: None
				};

				if let Some(id) = swapped_id.filter(|_| is_tmux()) {
					run_silent_action(action)
						.map(|()| *img = MaybeTransferred::Transferred(id))
						.map_err(DisplayErrSource::Transmission)
				} else {
					run_action(action, ev_stream)
						.await
						.map_err(DisplayErrSource::Transmission)
						.and_then(|img_id| {
							img_id
								.map(|id| *img = MaybeTransferred::Transferred(id))
								.ok_or(DisplayErrSource::KittageReturnedNoId)
						})
				}
			}
			MaybeTransferred::Transferred(image_id) => {
				let action = Action::Display {
					image_id: *image_id,
					placement_id: *image_id,
					config
				};
				if is_tmux() && !*tmux_focused.get_or_insert_with(is_tmux_pane_focused) {
					run_silent_action(action).map_err(DisplayErrSource::Transmission)
				} else {
					run_action(action, ev_stream)
						.await
						// don't need the return id 'cause we already know it
						.map(|_: Option<ImageId>| ())
						.map_err(DisplayErrSource::Transmission)
				}
			}
		};

		log::debug!("this_err is {this_err:#?}");

		if let Err(e) = this_err {
			match err.as_mut() {
				Ok(()) => err = Err((SmallVec::from([page_num].as_slice()), e)),
				Err((v, _)) => v.push(page_num)
			}
		}
	}

	let z_idxes_to_remove = *last_z_index;
	*last_z_index = new_z_index;

	let clear_res = run_action(
		Action::Delete(DeleteConfig {
			effect: ClearOrDelete::Clear,
			which: WhichToDelete::PlacementsWithZIndex(z_idxes_to_remove)
		}),
		ev_stream
	)
	.await;

	match err {
		Err((failed_pages, source)) => Err(DisplayErr {
			failed_pages,
			user_facing_err: "Couldn't transfer image to the terminal",
			source
		}),
		Ok(()) => clear_res
			.map_err(|e| DisplayErr::empty("Couldn't clear previously-sent images", e))
			.map(|_| ())
	}
}

#[cfg(test)]
mod tests {
	use kittage::display::DisplayConfig;

	use super::*;

	#[test]
	fn silent_header_patcher_injects_q2_for_display_and_delete() {
		let mut out = Vec::new();
		let action = Action::Display {
			image_id: NonZeroU32::new(82).unwrap(),
			placement_id: NonZeroU32::new(82).unwrap(),
			config: DisplayConfig {
				location: DisplayLocation::default(),
				cursor_movement: CursorMovementPolicy::DontMove,
				..DisplayConfig::default()
			}
		};
		let patcher = SilentHeaderPatcher::new(&mut out);
		action
			.write_transmit_to(patcher, Verbosity::Silent)
			.unwrap();
		assert!(
			out.starts_with(b"\x1b_Ga=p,q=2,i=82,p=82"),
			"unexpected Display bytes: {:?}",
			String::from_utf8_lossy(&out)
		);

		let mut del_out = Vec::new();
		let del = Action::Delete(DeleteConfig {
			effect: ClearOrDelete::Clear,
			which: WhichToDelete::All
		});
		let patcher = SilentHeaderPatcher::new(&mut del_out);
		del.write_transmit_to(patcher, Verbosity::Silent).unwrap();
		assert_eq!(del_out, b"\x1b_Ga=d,q=2,d=a\x1b\\");
	}

	#[test]
	fn silent_header_patcher_handles_byte_by_byte_and_passthrough() {
		let mut out = Vec::new();
		let mut patcher = SilentHeaderPatcher::new(&mut out);
		for &b in b"\x1b_Ga=p,i=1\x1b\\" {
			patcher.write_all(&[b]).unwrap();
		}
		patcher.flush().unwrap();
		assert_eq!(out, b"\x1b_Ga=p,q=2,i=1\x1b\\");

		let mut passthrough = Vec::new();
		let mut patcher = SilentHeaderPatcher::new(&mut passthrough);
		patcher.write_all(b"\x1b_Ga=T,q=2,i=82;AAAA\x1b\\").unwrap();
		patcher.flush().unwrap();
		assert_eq!(passthrough, b"\x1b_Ga=T,q=2,i=82;AAAA\x1b\\");
	}

	#[test]
	fn parse_tmux_pane_state_classifies_focus_visible_and_hidden() {
		assert_eq!(parse_tmux_pane_state("1 1 1 0 0\n"), TmuxPaneState::Focused);
		assert_eq!(parse_tmux_pane_state("1 1 1 1 0\n"), TmuxPaneState::Focused);
		assert_eq!(
			parse_tmux_pane_state("1 1 0 0 0\n"),
			TmuxPaneState::VisibleUnfocused
		);
		assert_eq!(parse_tmux_pane_state("1 1 0 1 0\n"), TmuxPaneState::Hidden);
		assert_eq!(parse_tmux_pane_state("1 0 1 0 0\n"), TmuxPaneState::Hidden);
		assert_eq!(parse_tmux_pane_state("0 1 1 0 0\n"), TmuxPaneState::Hidden);
		assert_eq!(parse_tmux_pane_state("1 1 1 0 1\n"), TmuxPaneState::Hidden);
		assert_eq!(parse_tmux_pane_state(""), TmuxPaneState::Hidden);
	}

	#[test]
	fn parse_tmux_cell_size_parses_positive_dimensions() {
		assert_eq!(parse_tmux_cell_size("12 25\n"), Some((12, 25)));
		assert_eq!(parse_tmux_cell_size("0 25\n"), None);
		assert_eq!(parse_tmux_cell_size("12 0\n"), None);
		assert_eq!(parse_tmux_cell_size("12\n"), None);
		assert_eq!(parse_tmux_cell_size(""), None);
	}
}
