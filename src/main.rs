use core::{
	error::Error,
	num::{NonZeroU32, NonZeroUsize}
};
use std::{
	borrow::Cow,
	ffi::OsString,
	io::{BufReader, IsTerminal as _, Read as _, Stdout, Write as _, stdout},
	mem,
	path::PathBuf,
	sync::{Arc, Mutex},
	time::Duration
};

use crossterm::{
	event::{DisableFocusChange, EnableFocusChange, Event as CrosstermEvent, EventStream},
	execute,
	terminal::{
		EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
		enable_raw_mode, window_size
	}
};
use debounce::EventDebouncer;
use flexi_logger::FileSpec;
use flume::{Sender, r#async::RecvStream};
use futures_util::{FutureExt as _, stream::StreamExt as _};
use kittage::{
	action::Action,
	delete::{ClearOrDelete, DeleteConfig, WhichToDelete},
	error::{TerminalError, TransmitError}
};
use notify::{Event, EventKind, RecursiveMode, Watcher as _};
use ratatui::{Terminal, backend::CrosstermBackend};
use ratatui_image::{
	FontSize,
	picker::{Picker, ProtocolType}
};
use tdf::{
	PrerenderLimit,
	converter::{ConvertedPage, ConverterMsg, run_conversion_loop},
	kitty::{
		DisplayErr, DisplayErrSource, KittyDisplay, TmuxPaneState, clear_kitty_placements_silent,
		display_kitty_images, do_shms_work, ensure_tmux_passthrough_all, is_tmux, run_action,
		tmux_client_cell_size, tmux_pane_state
	},
	renderer::{self, MUPDF_BLACK, MUPDF_WHITE, RenderError, RenderInfo, RenderNotif},
	tui::{BottomMessage, InputAction, MessageSetting, Tui}
};

// Dummy struct for easy errors in main
struct WrappedErr(Cow<'static, str>);

impl std::fmt::Display for WrappedErr {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

impl std::fmt::Debug for WrappedErr {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		std::fmt::Display::fmt(self, f)
	}
}

impl std::error::Error for WrappedErr {}

fn reset_term() {
	_ = disable_raw_mode();
	if tdf::kitty::is_tmux() {
		let mut out = std::io::stdout().lock();
		_ = out.write_all(b"\x1bPtmux;\x1b\x1b_Ga=d,d=A,q=2\x1b\x1b\\\x1b\\");
		_ = out.flush();
	}
	_ = execute!(
		std::io::stdout(),
		LeaveAlternateScreen,
		crossterm::cursor::Show,
		crossterm::event::DisableMouseCapture,
		DisableFocusChange
	);
}

fn main() -> Result<(), WrappedErr> {
	let rt = tokio::runtime::Builder::new_multi_thread()
		.worker_threads(3)
		.enable_io()
		.enable_time()
		.build()
		.unwrap();

	rt.block_on(async move {
		let result = inner_main().await;
		reset_term();
		result
	})
}

async fn inner_main() -> Result<(), WrappedErr> {
	#[cfg(unix)]
	let mut usr1_sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
		.map_err(|e| WrappedErr(format!("Can't install SIGUSR1 handler: {e}").into()))?;
	#[cfg(not(unix))]
	let mut usr1_sig = ();

	let hook = std::panic::take_hook();
	std::panic::set_hook(Box::new(move |info| {
		if std::thread::current().name() != Some("tdf-renderer") {
			reset_term();
			hook(info);
		}
	}));

	#[cfg(feature = "tracing")]
	console_subscriber::init();

	const DEFAULT_DEBOUNCE_DELAY: Duration = Duration::from_millis(50);

	let flags = xflags::parse_or_exit! {
		/// Display the pdf with the pages starting at the right hand size and moving left and
		/// adjust input keys to match
		optional -r,--r-to-l
		/// The maximum number of pages to display together, horizontally, at a time
		optional -m,--max-wide max_wide: NonZeroUsize
		/// Fullscreen the pdf (hide document name, page count, etc)
		optional -f,--fullscreen
		/// The time to wait for the file to stop changing before reloading, in milliseconds.
		/// Defaults to 50ms.
		optional --reload-delay reload_delay: u64
		/// The number of pages to prerender surrounding the currently-shown page; 0 means no
		/// limit. Defaults to 25.
		optional -p,--prerender prerender: usize
		/// Custom white color, specified in css format (e.g. "FFFFFF" or "rgb(255, 255, 255)")
		optional -w,--white-color white: String
		/// Custom black color, specified in css format (e.g "000000" or "rgb(0, 0, 0)")
		optional -b,--black-color black: String
		/// Use terminal foreground/background colors for the PDF
		optional -t,--terminal-colors
		/// Print the version and exit
		optional --version
		/// PDF file to read
		optional file: PathBuf
	};

	if flags.version {
		println!("{}", env!("CARGO_PKG_VERSION"));
		return Ok(());
	}

	let Some(file) = flags.file else {
		return Err(WrappedErr(
			"Please specify the file to open, e.g. `tdf ./my_example_pdf.pdf`".into()
		));
	};

	let path = file
		.canonicalize()
		.map_err(|e| WrappedErr(format!("Cannot canonicalize provided file: {e}").into()))?;

	if flags.terminal_colors && (flags.black_color.is_some() || flags.white_color.is_some()) {
		return Err(WrappedErr(
			"--terminal-colors cannot be combined with --black-color or --white-color".into()
		));
	}

	let (black, white) = if flags.terminal_colors {
		query_terminal_colors()
	} else {
		let black = flags
			.black_color
			.as_deref()
			.map(|color| {
				parse_color_to_i32(color).map_err(|e| {
					WrappedErr(
						format!(
							"Couldn't parse black color {color:?}: {e} - is it formatted like a CSS color?"
						)
						.into()
					)
				})
			})
			.transpose()?
			.unwrap_or(MUPDF_BLACK);

		let white = flags
			.white_color
			.as_deref()
			.map(|color| {
				parse_color_to_i32(color).map_err(|e| {
					WrappedErr(
						format!(
							"Couldn't parse white color {color:?}: {e} - is it formatted like a CSS color?"
						)
						.into()
					)
				})
			})
			.transpose()?
			.unwrap_or(MUPDF_WHITE);

		(black, white)
	};

	// need to keep it around throughout the lifetime of the program, but don't rly need to use it.
	// Just need to make sure it doesn't get dropped yet.
	let maybe_logger = if std::env::var("RUST_LOG").is_ok() {
		Some(
			flexi_logger::Logger::try_with_env()
				.map_err(|e| WrappedErr(format!("Couldn't create initial logger: {e}").into()))?
				.log_to_file(FileSpec::try_from("./debug.log").map_err(|e| {
					WrappedErr(format!("Couldn't create FileSpec for logger: {e}").into())
				})?)
				.start()
				.map_err(|e| WrappedErr(format!("Can't start logger: {e}").into()))?
		)
	} else {
		None
	};

	let (watch_to_render_tx, render_rx) = flume::unbounded();
	let to_renderer = watch_to_render_tx.clone();

	let (render_tx, tui_rx) = flume::unbounded();
	let watch_to_tui_tx = render_tx.clone();

	let mut watcher = notify::recommended_watcher(on_notify_ev(
		watch_to_tui_tx,
		watch_to_render_tx,
		path.file_name()
			.ok_or_else(|| WrappedErr("Path does not have a last component??".into()))?
			.to_owned(),
		flags
			.reload_delay
			.map_or(DEFAULT_DEBOUNCE_DELAY, Duration::from_millis)
	))
	.map_err(|e| WrappedErr(format!("Couldn't start watching the provided file: {e}").into()))?;

	// So we have to watch the parent directory of the file that we are interested in because the
	// `notify` library works on inodes, and if the file is deleted, that inode is gone as well,
	// and then the notify library just gives up on trying to watch for the file reappearing. Imo
	// they should start watching the parent directory if the file is deleted, and then wait for it
	// to reappear and then begin watching it again, but whatever. It seems they've made their
	// opinion on this clear
	// (https://github.com/notify-rs/notify/issues/113#issuecomment-281836995) so whatever, guess
	// we have to do this annoying workaround.
	watcher
		.watch(
			path.parent().expect("The root directory is not a PDF"),
			RecursiveMode::NonRecursive
		)
		.map_err(|e| WrappedErr(format!("Can't watch the provided file: {e}").into()))?;

	execute!(
		std::io::stdout(),
		EnterAlternateScreen,
		crossterm::cursor::Hide,
		crossterm::event::EnableMouseCapture,
		EnableFocusChange
	)
	.map_err(|e| {
		WrappedErr(
			format!(
				"Couldn't enter the alternate screen and hide the cursor for proper presentation: {e}"
			)
			.into()
		)
	})?;

	// We need to create `picker` on this thread because if we create it on the `renderer` thread,
	// it messes up something with user input. Input never makes it to the crossterm thing.
	// Query it before spawning `tdf-renderer` so renderer, converter, and TUI share the exact same cell size.
	let picker = Picker::from_query_stdio().unwrap_or_else(|_| {
		#[expect(deprecated)]
		Picker::from_fontsize(resolve_fallback_font_size())
	});

	let mut font_size = picker.font_size();
	font_size.width = font_size.width.max(1);
	font_size.height = font_size.height.max(1);

	let is_kitty = picker.protocol_type() == ProtocolType::Kitty;
	if is_kitty {
		ensure_tmux_passthrough_all();
	}

	let file_name = path.file_name().map_or_else(
		|| "Unknown file".into(),
		|n| n.to_string_lossy().to_string()
	);
	let tui = Tui::new(file_name, path.clone(), flags.max_wide, flags.r_to_l, is_kitty);
	let startup = tui.startup_sync();

	// then we want to spawn off the rendering task
	// We need to use the thread::spawn API so that this exists in a thread not owned by tokio,
	// since the methods we call in `start_rendering` will panic if called in an async context
	let prerender = match flags.prerender {
		None => PrerenderLimit::Limited(NonZeroUsize::new(25).unwrap()),
		Some(0) => PrerenderLimit::All,
		Some(n) => PrerenderLimit::Limited(NonZeroUsize::new(n).unwrap())
	};

	let file_path = path.clone();
	std::thread::Builder::new()
		.name("tdf-renderer".to_string())
		.spawn(move || {
			renderer::start_rendering(
				&file_path,
				render_tx,
				render_rx,
				font_size.height,
				font_size.width,
				prerender,
				black,
				white,
				startup.page,
				startup.invert,
				startup.rotate_steps,
				startup.fit_or_fill
			)
		})
		.map_err(|e| WrappedErr(format!("Failed to spawn renderer thread: {e}").into()))?;

	let mut ev_stream = crossterm::event::EventStream::new();

	let (to_converter, from_main) = flume::unbounded();
	let (to_main, from_converter) = flume::unbounded();

	let shms_work = is_kitty && do_shms_work(&mut ev_stream).await;

	tokio::spawn(run_conversion_loop(
		to_main, from_main, picker, 20, shms_work
	));

	let backend = CrosstermBackend::new(std::io::stdout());
	let mut term = Terminal::new(backend).map_err(|e| {
		WrappedErr(format!("Couldn't set up crossterm's terminal backend: {e}").into())
	})?;
	// term.skip_diff(true);

	enable_raw_mode().map_err(|e| {
		WrappedErr(
			format!("Can't enable raw mode, which is necessary to receive input: {e}").into()
		)
	})?;

	if is_kitty {
		run_action(
			Action::Delete(DeleteConfig {
				effect: ClearOrDelete::Delete,
				which: WhichToDelete::IdRange(NonZeroU32::new(1).unwrap()..=NonZeroU32::MAX)
			}),
			&mut ev_stream
		)
		.await
		.map_err(|e| {
			WrappedErr(format!("Couldn't delete all previous images from memory: {e}").into())
		})?;
	}

	let fullscreen = flags.fullscreen;
	let main_area = Tui::main_layout(&term.get_frame(), fullscreen);
	to_renderer
		.send(RenderNotif::Area(main_area.page_area))
		.map_err(|e| {
			WrappedErr(
				format!("Couldn't inform the rendering thread of the available area: {e}").into()
			)
		})?;
	if startup.page > 0 {
		let _ = to_converter.send(ConverterMsg::GoToPage(startup.page));
	}

	let tui_rx = tui_rx.into_stream();
	let from_converter = from_converter.into_stream();

	enter_redraw_loop(
		ev_stream,
		to_renderer,
		tui_rx,
		to_converter,
		from_converter,
		fullscreen,
		is_kitty,
		tui,
		&mut term,
		main_area,
		font_size,
		&mut usr1_sig
	)
	.await
	.map_err(|e| {
		WrappedErr(
			format!(
				"An unexpected error occurred while communicating between different parts of tdf: {e}"
			)
			.into()
		)
	})?;

	drop(maybe_logger);
	Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TmuxVisibilityAction {
	None,
	ClearAndDefer,
	Restore
}

fn decide_tmux_visibility_action(
	state: TmuxPaneState,
	kitty_images_on_screen: bool,
	pending_kitty_display: bool
) -> TmuxVisibilityAction {
	match state {
		TmuxPaneState::Hidden if kitty_images_on_screen => TmuxVisibilityAction::ClearAndDefer,
		TmuxPaneState::Focused | TmuxPaneState::VisibleUnfocused if pending_kitty_display => {
			TmuxVisibilityAction::Restore
		}
		_ => TmuxVisibilityAction::None
	}
}

fn sync_tmux_kitty_visibility(
	tui: &mut Tui,
	kitty_images_on_screen: &mut bool,
	pending_kitty_display: &mut bool
) -> bool {
	match decide_tmux_visibility_action(
		tmux_pane_state(),
		*kitty_images_on_screen,
		*pending_kitty_display
	) {
		TmuxVisibilityAction::ClearAndDefer => {
			clear_kitty_placements_silent();
			*kitty_images_on_screen = false;
			*pending_kitty_display = true;
			tui.invalidate_render();
			false
		}
		TmuxVisibilityAction::Restore => {
			tui.invalidate_render();
			true
		}
		TmuxVisibilityAction::None => false
	}
}

#[cfg(unix)]
async fn recv_usr1(sig: &mut tokio::signal::unix::Signal) -> Option<()> {
	sig.recv().await
}

#[cfg(not(unix))]
async fn recv_usr1(_sig: &mut ()) -> Option<()> {
	std::future::pending().await
}

// oh shut up clippy who cares
#[expect(clippy::too_many_arguments)]
async fn enter_redraw_loop(
	mut ev_stream: EventStream,
	to_renderer: Sender<RenderNotif>,
	mut tui_rx: RecvStream<'_, Result<RenderInfo, RenderError>>,
	to_converter: Sender<ConverterMsg>,
	mut from_converter: RecvStream<'_, Result<ConvertedPage, RenderError>>,
	mut fullscreen: bool,
	is_kitty: bool,
	mut tui: Tui,
	term: &mut Terminal<CrosstermBackend<Stdout>>,
	mut main_area: tdf::tui::RenderLayout,
	font_size: FontSize,
	#[cfg(unix)] usr1_sig: &mut tokio::signal::unix::Signal,
	#[cfg(not(unix))] usr1_sig: &mut ()
) -> Result<(), Box<dyn Error>> {
	let mut kitty_z_idx = i32::MIN;
	let use_tmux_kitty_guard = is_kitty && is_tmux();
	let mut kitty_images_on_screen = false;
	let mut pending_kitty_display = false;
	let mut vis_tick = tokio::time::interval(Duration::from_millis(350));
	vis_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

	loop {
		let mut needs_redraw = true;
		let next_ev = ev_stream.next().fuse();
		tokio::select! {
			// First we check if we have any keystrokes
			Some(ev) = next_ev => {
				let Ok(ev) = ev else {
					tui.save_state();
					return Ok(());
				};

				match ev {
					CrosstermEvent::FocusLost => {
						needs_redraw = if use_tmux_kitty_guard {
							sync_tmux_kitty_visibility(
								&mut tui,
								&mut kitty_images_on_screen,
								&mut pending_kitty_display
							)
						} else {
							false
						};
					}
					CrosstermEvent::FocusGained => {
						if use_tmux_kitty_guard && pending_kitty_display {
							tui.invalidate_render();
							needs_redraw = true;
						} else {
							needs_redraw = false;
						}
					}
					_ => match tui.handle_event(&ev) {
						None => needs_redraw = false,
						Some(action) => match action {
							InputAction::Redraw => (),
							InputAction::QuitApp => {
								tui.save_state();
								return Ok(());
							},
							InputAction::JumpingToPage(page) => {
								to_renderer.send(RenderNotif::JumpToPage(page))?;
								to_converter.send(ConverterMsg::GoToPage(page))?;
							},
							InputAction::Search(term) => to_renderer.send(RenderNotif::Search(term))?,
							InputAction::Invert => to_renderer.send(RenderNotif::Invert)?,
							InputAction::Rotate => to_renderer.send(RenderNotif::Rotate)?,
							InputAction::Reload => to_renderer.send(RenderNotif::Reload)?,
							InputAction::Fullscreen => fullscreen = !fullscreen,
							InputAction::SwitchRenderZoom(f_or_f) => {
								to_renderer.send(RenderNotif::SwitchFitOrFill(f_or_f))?;
							},
							InputAction::ExtractPageText { page, open_visual } => {
								to_renderer.send(RenderNotif::ExtractPageText { page, open_visual })?;
							}
						}
					}
				}
			},
			Some(()) = recv_usr1(usr1_sig), if use_tmux_kitty_guard => {
				needs_redraw = sync_tmux_kitty_visibility(
					&mut tui,
					&mut kitty_images_on_screen,
					&mut pending_kitty_display
				);
			},
			_ = vis_tick.tick(), if use_tmux_kitty_guard && (kitty_images_on_screen || pending_kitty_display) => {
				needs_redraw = sync_tmux_kitty_visibility(
					&mut tui,
					&mut kitty_images_on_screen,
					&mut pending_kitty_display
				);
			},
			Some(renderer_msg) = tui_rx.next() => {
				match renderer_msg {
					Ok(render_info) => match render_info {
						RenderInfo::NumPages(num) => {
							tui.set_n_pages(num);
							to_converter.send(ConverterMsg::NumPages(num))?;
						},
						RenderInfo::Page(info) => {
							tui.got_num_results_on_page(info.page_num, info.result_rects.len());
							to_converter.send(ConverterMsg::AddImg(info))?;
						},
						RenderInfo::Reloaded => tui.set_msg(MessageSetting::Some(BottomMessage::Reloaded)),
						RenderInfo::SearchResults { page_num, num_results } =>
							tui.got_num_results_on_page(page_num, num_results),
						RenderInfo::DocMetadata { outlines, info } =>
							tui.set_doc_metadata(outlines, info),
						RenderInfo::PageLinks { page_num, links } =>
							tui.set_page_links(page_num, links),
						RenderInfo::PageText { page_num, text, open_visual } =>
							tui.got_page_text(page_num, text, open_visual),
					},
					Err(e) => tui.show_error(e),
				}
			}
			Some(img_res) = from_converter.next() => {
				match img_res {
					Ok(ConvertedPage { page, num, num_results }) => {
						tui.page_ready(page, num, num_results);
						while let Some(dropped_page) = tui.evict_far_page() {
							to_renderer.send(RenderNotif::PageDropped(dropped_page))?;
						}
						if num == tui.page {
							needs_redraw = true;
						}
					},
					Err(e) => tui.show_error(e),
				}
			},
		};

		let _ = term.autoresize();
		let new_area = Tui::main_layout(&term.get_frame(), fullscreen);
		if new_area != main_area {
			main_area = new_area;
			tui.invalidate_render();
			to_renderer.send(RenderNotif::Area(main_area.page_area))?;
			needs_redraw = true;
		}

		if needs_redraw {
			let mut to_display = KittyDisplay::NoChange;
			term.draw(|f| {
				to_display = tui.render(f, &main_area, font_size);
			})?;

			if use_tmux_kitty_guard {
				let pane_state = tmux_pane_state();
				if matches!(to_display, KittyDisplay::DisplayImages(_))
					&& pane_state == TmuxPaneState::Hidden
				{
					pending_kitty_display = true;
					if kitty_images_on_screen {
						clear_kitty_placements_silent();
						kitty_images_on_screen = false;
					}
					to_display = KittyDisplay::NoChange;
				} else {
					match &to_display {
						KittyDisplay::DisplayImages(_) => {
							kitty_images_on_screen = true;
							pending_kitty_display = false;
						}
						KittyDisplay::ClearImages => {
							kitty_images_on_screen = false;
						}
						KittyDisplay::NoChange => ()
					}
				}
			}

			let maybe_err =
				display_kitty_images(to_display, &mut ev_stream, &mut kitty_z_idx).await;

			if let Err(DisplayErr {
				failed_pages,
				user_facing_err,
				source
			}) = maybe_err
			{
				match source {
					// This is the error that kitty & ghostty provide us when they delete an
					// image due to memory constraints, so if we get it, we just fix it by
					// re-rendering so it don't display it to the user
					//
					// [TODO] maybe when we detect that an image was deleted, we probe the
					// terminal for the pages around it to see if they were deleted too and if
					// they were, we re-render them? idk
					DisplayErrSource::Transmission(TransmitError::Terminal(
						TerminalError::NoEntity(_)
					)) => (),
					_ => tui.set_msg(MessageSetting::Some(BottomMessage::Error(format!(
						"{user_facing_err}: {source}"
					))))
				}

				// Never re-queue pages when reading the terminal reply fails/times out (e.g. when
				// tmux routes the reply to another pane after a rapid focus switch), otherwise each
				// timeout restarts a re-render/re-transmit loop.
				if !matches!(
					source,
					DisplayErrSource::Transmission(TransmitError::ReadingInput(_))
				) {
					for page_num in failed_pages {
						tui.page_failed_display(page_num);
						// So that they get re-rendered and sent over again
						to_renderer.send(RenderNotif::PageNeedsReRender(page_num))?;
					}
				}
			}

			execute!(stdout().lock(), EndSynchronizedUpdate)?;
		}
	}
}

fn on_notify_ev(
	to_tui_tx: flume::Sender<Result<RenderInfo, RenderError>>,
	to_render_tx: flume::Sender<RenderNotif>,
	file_name: OsString,
	debounce_delay: Duration
) -> impl Fn(notify::Result<Event>) {
	let last_event: Mutex<Result<(), RenderError>> = Mutex::new(Ok(()));
	let last_event = Arc::new(last_event);

	let debouncer = EventDebouncer::new(debounce_delay, {
		let last_event = last_event.clone();
		move |()| {
			let event = mem::replace(&mut *last_event.lock().unwrap(), Ok(()));
			match event {
				// This shouldn't fail to send unless the receiver gets disconnected. If that's
				// happened, then like the main thread has panicked or something, so it doesn't matter
				// we don't handle the error here.
				Ok(()) => to_render_tx.send(RenderNotif::Reload).unwrap(),
				// If we get an error here, and then an error sending, everything's going wrong. Just give
				// up lol.
				Err(e) => to_tui_tx.send(Err(e)).unwrap()
			}
		}
	});

	move |res| {
		let event = match res {
			Err(e) => Err(RenderError::Notify(e)),

			// TODO: Should we match EventKind::Rename and propogate that so that the other parts of the
			// process know that too? Or should that be
			Ok(ev) => {
				// We only watch the parent directory (see the comment above `watcher.watch` in `fn
				// main`) so we need to filter out events to only ones that pertain to the single file
				// we care about
				if !ev
					.paths
					.iter()
					.any(|path| path.file_name().is_some_and(|f| f == file_name))
				{
					return;
				}

				match ev.kind {
					EventKind::Access(_) => return,
					EventKind::Remove(_) => Err(RenderError::Converting("File was deleted".into())),
					EventKind::Other
					| EventKind::Any
					| EventKind::Create(_)
					| EventKind::Modify(_) => Ok(())
				}
			}
		};
		*last_event.lock().unwrap() = event;
		debouncer.put(());
	}
}

fn parse_color_to_i32(cs: &str) -> Result<i32, csscolorparser::ParseColorError> {
	let color = csscolorparser::parse(cs)?;
	let [r, g, b, _] = color.to_rgba8();
	Ok(i32::from_be_bytes([0, r, g, b]))
}

fn query_terminal_colors() -> (i32, i32) {
	if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
		return (MUPDF_BLACK, MUPDF_WHITE);
	}

	let Ok(()) = enable_raw_mode() else {
		return (MUPDF_BLACK, MUPDF_WHITE);
	};

	struct RawModeGuard;
	impl Drop for RawModeGuard {
		fn drop(&mut self) {
			let _ = disable_raw_mode();
		}
	}
	let _guard = RawModeGuard;

	let stdin = std::io::stdin();
	let mut handle = stdin.lock();

	let fg = query_osc_color(10, &mut handle);
	let bg = query_osc_color(11, &mut handle);
	drop(handle);

	(fg.unwrap_or(MUPDF_BLACK), bg.unwrap_or(MUPDF_WHITE))
}

fn query_osc_color(osc: u8, handle: &mut std::io::StdinLock<'_>) -> Option<i32> {
	print!("\x1b]{osc};?\x1b\\");
	std::io::stdout().flush().ok()?;

	// response looks like "\u{1b}]10;rgb:rrrr/bbbb/gggg\u{1b}\\" or "\u{1b}]10;rgb:rr/bb/gg\u{1b}\\"
	// or if osc is 11, "\u{1b}]11;rgb:rrrr/bbbb/gggg\u{1b}\\" or "\u{1b}]11;rgb:rr/bb/gg\u{1b}\\"

	// We expect the response to be either 19 or 25 bytes.
	let mut response_buf = [0u8; 25];

	handle.read_exact(&mut response_buf[..19])
		.inspect_err(|e| eprintln!("Couldn't get a response from your terminal for querying OSC {osc}; please file a bug with tdf with your terminal emulator (underlying err: {e})"))
		.ok()?;

	let two_digit_colors = response_buf[11] == b'/';

	if !two_digit_colors {
		handle.read_exact(&mut response_buf[19..])
			.inspect_err(|e| eprintln!("Couldn't get a response from your terminal for querying OSC {osc}; please file a bug with tdf with your terminal emulator (underlying err: {e})"))
			.ok()?;
	}

	osc_response_buf_to_color(response_buf)
}

fn osc_response_buf_to_color(response_buf: [u8; 25]) -> Option<i32> {
	fn parse(ascii_hex_bytes: &[u8]) -> Option<u8> {
		let mut color = 0;

		for byte in ascii_hex_bytes {
			color = (color << 4)
				+ (match byte {
					b'0'..=b'9' => byte - b'0',
					b'A'..=b'F' => byte - (b'A' - 10),
					b'a'..=b'f' => byte - (b'a' - 10),
					_ => return None
				});
		}

		Some(color)
	}

	let two_digit_colors = response_buf[11] == b'/';

	// this is minimimal validation.
	// the response either has `rrrr/gggg/bbbb` or `rr/gg/bb` starting at byte 9.
	// So we grab the first two digits out of the r, g, and b sections (since we can only support
	// 8-bit colors with mupdf; we can't do the 16-bits that the terminal might respond with), we
	// parse them, and we return it as an i32.
	let r = parse(&response_buf[9..11])?;
	let g = parse(&response_buf[if two_digit_colors { 12..14 } else { 14..16 }])?;
	let b = parse(&response_buf[if two_digit_colors { 15..17 } else { 19..21 }])?;

	Some(i32::from_be_bytes([0, r, g, b]))
}

fn resolve_fallback_font_size() -> FontSize {
	if is_tmux()
		&& let Some((width, height)) = tmux_client_cell_size()
	{
		return FontSize { width, height };
	}

	if let Ok(mut ws) = window_size() {
		// CSI 14 t returns the entire terminal window pixel size in tmux, not the pane's, so
		// dividing it by split-pane columns/rows would inflate cell dimensions.
		if (ws.width == 0 || ws.height == 0)
			&& !is_tmux()
			&& let Ok((w, h)) = get_font_size_through_stdio()
		{
			ws.width = w;
			ws.height = h;
		}
		if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 {
			return FontSize {
				width: (ws.width / ws.columns).max(1),
				height: (ws.height / ws.rows).max(1)
			};
		}
	}

	FontSize {
		width: 8,
		height: 16
	}
}

fn get_font_size_through_stdio() -> Result<(u16, u16), WrappedErr> {
	// send the command code to get the terminal window size
	print!("\x1b[14t");
	std::io::stdout().flush().unwrap();

	// we need to enable raw mode here since this bit of output won't print a newline; it'll
	// just print the info it wants to tell us. So we want to get all characters as they come
	enable_raw_mode().map_err(|e| {
		WrappedErr(
			format!("Can't enable raw mode, which is necessary to receive input: {e}").into()
		)
	})?;

	// read in the returned size until we hit a 't' (which indicates to us it's done)
	let input_vec = BufReader::new(std::io::stdin())
		.bytes()
		.filter_map(Result::ok)
		.take_while(|b| *b != b't')
		.collect::<Vec<_>>();

	// and then disable raw mode again in case we return an error in this next section
	disable_raw_mode().map_err(|e| {
		WrappedErr(format!("Can't put the terminal back into a normal input state: {e}").into())
	})?;

	let input_line = String::from_utf8(input_vec).map_err(|e| {
		WrappedErr(
			format!(
				"The terminal responded to our request for its font size by providing non-utf8 data: {e}"
			)
			.into()
		)
	})?;
	let input_line = input_line
		.trim_start_matches("\x1b[4")
		.trim_start_matches(';');

	// it should input it to us as `\e[4;<height>;<width>t`, so we need to split to get the h/w
	// ignore the first val
	let mut splits = input_line.split([';', 't']);

	let (Some(h), Some(w)) = (splits.next(), splits.next()) else {
		return Err(WrappedErr(
			format!("Terminal responded with unparseable size response '{input_line}'").into()
		));
	};

	let h = h.parse::<u16>().map_err(|e| {
		WrappedErr(
			format!(
				"Your terminal said its height is {h}, but that is not a 16-bit unsigned integer: {e}"
			)
			.into()
		)
	})?;
	let w = w.parse::<u16>().map_err(|e| {
		WrappedErr(
			format!(
				"Your terminal said its width is {w}, but that is not a 16-bit unsigned integer: {e}"
			)
			.into()
		)
	})?;

	Ok((w, h))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn osc_response_parsing() {
		fn eq(bytes: [u8; 25], r: u8, g: u8, b: u8) {
			let resp = osc_response_buf_to_color(bytes);
			assert_eq!(resp, Some(i32::from_be_bytes([0, r, g, b])));
		}

		eq(*b"\x1b]10;rgb:11/22/33\x1b\\000000", 0x11, 0x22, 0x33);
		eq(*b"\x1b]11;rgb:aa11/23bf/fff0\x1b1", 0xaa, 0x23, 0xff);
	}

	#[test]
	fn tmux_visibility_transitions_clear_on_hide_and_restore_when_visible() {
		assert_eq!(
			decide_tmux_visibility_action(TmuxPaneState::Hidden, true, false),
			TmuxVisibilityAction::ClearAndDefer
		);
		assert_eq!(
			decide_tmux_visibility_action(TmuxPaneState::Hidden, false, true),
			TmuxVisibilityAction::None
		);
		assert_eq!(
			decide_tmux_visibility_action(TmuxPaneState::VisibleUnfocused, false, true),
			TmuxVisibilityAction::Restore
		);
		assert_eq!(
			decide_tmux_visibility_action(TmuxPaneState::Focused, false, true),
			TmuxVisibilityAction::Restore
		);
		assert_eq!(
			decide_tmux_visibility_action(TmuxPaneState::VisibleUnfocused, true, false),
			TmuxVisibilityAction::None
		);
	}
}
