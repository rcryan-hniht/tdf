use std::{borrow::Cow, io::stdout, num::NonZeroUsize, path::PathBuf};

use crossterm::{
	event::{Event, KeyCode, KeyModifiers, MouseEventKind},
	execute,
	terminal::BeginSynchronizedUpdate
};
use kittage::display::DisplayLocation;
use ratatui::{
	Frame,
	layout::{Constraint, Flex, Layout, Position, Rect},
	prelude::{Line, Text},
	style::{Color, Modifier, Style},
	symbols::border,
	text::Span,
	widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap}
};
use ratatui_image::{FontSize, Image};

use crate::{
	FitOrFill,
	converter::{ConvertedImage, MaybeTransferred},
	kitty::{KittyDisplay, KittyReadyToDisplay, is_tmux},
	renderer::{RenderError, fill_default},
	skip::Skip,
	zathura::{
		BisectState, FileState, JumpList, LinkActionKind, ListPickerState, MarkPos, PageLink,
		PickerItem, PickerKind, TocNode, TocState, VisualTextState, complete_command,
		copy_to_clipboard, load_file_state, open_external_uri, save_file_state
	}
};

#[derive(Default)]
enum Overlay {
	#[default]
	None,
	Help,
	Index,
	Info,
	VisualText(VisualTextState),
	Picker(ListPickerState)
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum PendingPrefix {
	#[default]
	None,
	G,
	Z
}

pub struct Tui {
	name: String,
	file_path: PathBuf,
	pub page: usize,
	last_render: LastRender,
	bottom_msg: BottomMessage,
	prev_msg: Option<BottomMessage>,
	rendered: Vec<RenderedInfo>,
	page_constraints: PageConstraints,
	overlay: Overlay,
	is_kitty: bool,
	zoom: Option<Zoom>,
	state: FileState,
	jumplist: JumpList,
	bisect: BisectState,
	toc: TocState,
	doc_info: Vec<(String, String)>,
	last_search_term: String,
	search_backward: bool,
	num_prefix: Option<usize>,
	pending_prefix: PendingPrefix
}

#[derive(Default)]
struct LastRender {
	rect: Rect,
	pages_shown: usize,
	unused_width: u16
}

#[derive(Default)]
pub enum BottomMessage {
	#[default]
	Help,
	SearchResults(String),
	Info(String),
	Error(String),
	Input(InputCommand),
	Reloaded
}

pub enum InputCommand {
	Search { term: String, backward: bool },
	Command(String),
	MarkSet,
	MarkJump
}

struct PageConstraints {
	max_wide: Option<NonZeroUsize>,
	r_to_l: bool,
	first_page_solo: bool
}

#[derive(Default, Debug, Clone, Copy)]
struct Zoom {
	level: i16,
	cell_pan_from_left: u16,
	cell_pan_from_top: u16,
	max_pan_top: u16
}

impl Zoom {
	fn factor(self) -> f32 {
		const ZOOM_RATE: f32 = 1.1;
		const ZOOM_RATE_GRANULAR: f32 = 1.05;

		if self.level > 0 {
			ZOOM_RATE.powi(self.level.into())
		} else {
			ZOOM_RATE_GRANULAR.powi(self.level.into())
		}
	}

	fn step_in(&mut self) {
		self.level = self.level.saturating_add(1);
	}

	fn step_out(&mut self) {
		self.level = self.level.saturating_sub(1);
	}

	fn reset_level(&mut self) {
		self.level = 0;
	}

	const PAN_STEP_X: i16 = 6;
	const PAN_STEP_Y: i16 = 6;

	fn pan(&mut self, direction: Direction) {
		let step = if direction.is_vertical() {
			Self::PAN_STEP_Y
		} else {
			Self::PAN_STEP_X
		};
		self.pan_by(direction, step);
	}

	fn pan_by(&mut self, direction: Direction, step: i16) {
		let (target, sign) = match direction {
			Direction::Up => (&mut self.cell_pan_from_top, -1),
			Direction::Down => (&mut self.cell_pan_from_top, 1),
			Direction::Left => (&mut self.cell_pan_from_left, -1),
			Direction::Right => (&mut self.cell_pan_from_left, 1)
		};
		*target = target.saturating_add_signed(sign * step);
	}

	fn pan_page_top(&mut self) {
		self.cell_pan_from_top = 0;
	}

	fn pan_page_bottom(&mut self) {
		self.cell_pan_from_top = u16::MAX;
	}

	fn pan_left(&mut self) {
		self.cell_pan_from_left = 0;
	}

	fn pan_right(&mut self) {
		self.cell_pan_from_left = u16::MAX;
	}
}

#[derive(Clone, Copy, Debug)]
enum Direction {
	Up,
	Down,
	Left,
	Right
}

impl Direction {
	fn flip_mouse_xy(self) -> Self {
		match self {
			Self::Up => Self::Left,
			Self::Left => Self::Up,
			Self::Down => Self::Right,
			Self::Right => Self::Down
		}
	}

	fn is_vertical(self) -> bool {
		match self {
			Self::Up | Self::Down => true,
			Self::Left | Self::Right => false
		}
	}
}

#[derive(Default)]
pub struct RenderedInfo {
	img: Option<ConvertedImage>,
	num_results: Option<usize>,
	links: Vec<PageLink>
}

#[derive(PartialEq)]
pub struct RenderLayout {
	pub page_area: Rect,
	pub top_and_bottom: Option<(Rect, Rect)>
}

pub struct StartupSync {
	pub page: usize,
	pub invert: bool,
	pub rotate_steps: u8,
	pub fit_or_fill: FitOrFill
}

impl Tui {
	#[must_use]
	pub fn new(
		name: String,
		file_path: PathBuf,
		max_wide: Option<NonZeroUsize>,
		r_to_l: bool,
		is_kitty: bool
	) -> Self {
		let loaded = load_file_state(&file_path).unwrap_or_default();
		let effective_max_wide = max_wide.or_else(|| {
			if loaded.dual_page {
				NonZeroUsize::new(2)
			} else {
				NonZeroUsize::new(1)
			}
		});
		let zoom = if is_kitty {
			loaded.zoom_level.map(|level| Zoom {
				level,
				cell_pan_from_left: loaded.pan_left,
				cell_pan_from_top: loaded.pan_top,
				max_pan_top: u16::MAX
			})
		} else {
			None
		};

		Self {
			name,
			file_path,
			page: loaded.page,
			prev_msg: None,
			bottom_msg: BottomMessage::Help,
			last_render: LastRender::default(),
			rendered: vec![],
			page_constraints: PageConstraints {
				max_wide: effective_max_wide,
				r_to_l,
				first_page_solo: loaded.first_page_solo
			},
			overlay: Overlay::None,
			is_kitty,
			zoom,
			state: loaded,
			jumplist: JumpList::default(),
			bisect: BisectState::default(),
			toc: TocState::default(),
			doc_info: Vec::new(),
			last_search_term: String::new(),
			search_backward: false,
			num_prefix: None,
			pending_prefix: PendingPrefix::None
		}
	}

	#[must_use]
	pub fn startup_sync(&self) -> StartupSync {
		StartupSync {
			page: self.page,
			invert: self.state.invert,
			rotate_steps: self.state.rotate_steps,
			fit_or_fill: if self.zoom.is_some() {
				FitOrFill::Fill
			} else {
				FitOrFill::Fit
			}
		}
	}

	pub fn invalidate_render(&mut self) {
		self.last_render.rect = Rect::default();
	}

	pub fn save_state(&mut self) {
		self.state.page = self.page;
		self.state.dual_page = self
			.page_constraints
			.max_wide
			.is_some_and(|w| w.get() >= 2);
		self.state.first_page_solo = self.page_constraints.first_page_solo;
		if let Some(z) = self.zoom {
			self.state.zoom_level = Some(z.level);
			self.state.pan_left = z.cell_pan_from_left;
			self.state.pan_top = z.cell_pan_from_top;
		} else {
			self.state.zoom_level = None;
			self.state.pan_left = 0;
			self.state.pan_top = 0;
		}
		save_file_state(&self.file_path, &self.state);
	}

	fn current_mark_pos(&self) -> MarkPos {
		let (pan_left, pan_top) = self
			.zoom
			.map_or((0, 0), |z| (z.cell_pan_from_left, z.cell_pan_from_top));
		MarkPos {
			page: self.page,
			pan_left,
			pan_top
		}
	}

	fn jump_with_history(&mut self, target: MarkPos) -> Option<InputAction> {
		let cur = self.current_mark_pos();
		self.jumplist.record(cur);
		self.bisect.reset();
		self.apply_mark_pos(target)
	}

	fn apply_mark_pos(&mut self, target: MarkPos) -> Option<InputAction> {
		let clamped = if self.rendered.is_empty() {
			target.page
		} else {
			target.page.min(self.rendered.len() - 1)
		};
		self.page = clamped;
		self.last_render.rect = Rect::default();
		if let Some(ref mut z) = self.zoom {
			z.cell_pan_from_left = target.pan_left;
			z.cell_pan_from_top = target.pan_top;
			z.max_pan_top = u16::MAX;
		}
		self.save_state();
		Some(InputAction::JumpingToPage(clamped))
	}

	#[must_use]
	pub fn main_layout(frame: &Frame<'_>, fullscreened: bool) -> RenderLayout {
		if fullscreened {
			RenderLayout {
				page_area: frame.area(),
				top_and_bottom: None
			}
		} else {
			let layout = Layout::default()
				.constraints([
					Constraint::Length(3),
					Constraint::Fill(1),
					Constraint::Length(3)
				])
				.horizontal_margin(2)
				.vertical_margin(1)
				.split(frame.area());

			RenderLayout {
				page_area: layout[1],
				top_and_bottom: Some((layout[0], layout[2]))
			}
		}
	}

	fn render_zoomed<'s>(
		mut img_area: Rect,
		font_size: FontSize,
		zoom: &mut Zoom,
		img: &'s mut MaybeTransferred,
		page_num: usize,
		(img_cell_w, img_cell_h): (u16, u16),
		(img_px_w, img_px_h): (u32, u32)
	) -> KittyDisplay<'s> {
		if img_area.width == 0
			|| img_area.height == 0
			|| img_cell_w == 0
			|| img_cell_h == 0
			|| font_size.width == 0
			|| font_size.height == 0
		{
			return KittyDisplay::ClearImages;
		}

		let mut img_section_w = if img_px_w > 0 {
			img_px_w as f32 / f32::from(font_size.width)
		} else {
			f32::from(img_cell_w)
		};
		let mut img_section_h = if img_px_h > 0 {
			img_px_h as f32 / f32::from(font_size.height)
		} else {
			f32::from(img_cell_h)
		};

		let zoom_factor = zoom.factor();

		if zoom_factor >= 1.0 {
			img_section_w /= zoom_factor;
			img_section_h /= zoom_factor;
		} else {
			let initial_page_w = f32::from(img_area.width);
			let initial_page_h = f32::from(img_area.height);

			let img_page_w_ratio = img_section_w / initial_page_w;
			let img_page_h_ratio = img_section_h / initial_page_h;

			fn shrink_move_page(dim: &mut u16, pos: &mut u16, axis_zoom_factor: f32) {
				let old_dim = *dim;
				let clamped_factor = axis_zoom_factor.clamp(0.05, 1.0);
				*dim = ((f32::from(*dim) * clamped_factor) as u16).max(1).min(old_dim);

				*pos += old_dim.saturating_sub(*dim) / 2;
			}

			fn shrink_move_two_pass(
				zoom: &mut Zoom,
				dim: &mut u16,
				pos: &mut u16,
				zoom_factor: f32,
				axis_zoom_factor: f32
			) {
				let mut first_pass_dim = *dim;
				let mut first_pass_pos = *pos;
				let mut first_pass_zoom = *zoom;

				if first_pass_zoom.level.is_negative() {
					first_pass_zoom.step_in();
				}
				let min_axis_factor = (1.0 / axis_zoom_factor.max(0.05)).min(1.0);
				let first_pass_zoom_factor = first_pass_zoom.factor();
				shrink_move_page(
					&mut first_pass_dim,
					&mut first_pass_pos,
					first_pass_zoom_factor.max(min_axis_factor)
				);

				shrink_move_page(dim, pos, zoom_factor.max(min_axis_factor));

				if first_pass_dim == *dim && first_pass_pos == *pos {
					zoom.step_in();
				}
			}

			let (dim, pos, axis_zoom_factor) = if img_page_w_ratio < img_page_h_ratio {
				(&mut img_area.width, &mut img_area.x, img_page_h_ratio)
			} else {
				(&mut img_area.height, &mut img_area.y, img_page_w_ratio)
			};
			shrink_move_two_pass(zoom, dim, pos, zoom_factor, axis_zoom_factor);
		}

		{
			let page_area_w = f32::from(img_area.width.max(1));
			let page_area_h = f32::from(img_area.height.max(1));

			let img_page_w_ratio = img_section_w / page_area_w;
			let img_page_h_ratio = img_section_h / page_area_h;

			if img_page_w_ratio < img_page_h_ratio {
				img_section_h = page_area_h * img_page_w_ratio;
			} else {
				img_section_w = page_area_w * img_page_h_ratio;
			}
		}

		let max_pan_top = img_cell_h.saturating_sub(img_section_h.ceil() as u16);
		zoom.cell_pan_from_left = zoom
			.cell_pan_from_left
			.min(img_cell_w.saturating_sub(img_section_w.ceil() as u16));
		zoom.cell_pan_from_top = zoom.cell_pan_from_top.min(max_pan_top);
		zoom.max_pan_top = max_pan_top;

		let pan_x = u32::from(zoom.cell_pan_from_left) * u32::from(font_size.width);
		let pan_y = u32::from(zoom.cell_pan_from_top) * u32::from(font_size.height);
		let mut width = (img_section_w * f32::from(font_size.width)).round() as u32;
		let mut height = (img_section_h * f32::from(font_size.height)).round() as u32;
		if img_px_w > 0 {
			width = width.min(img_px_w.saturating_sub(pan_x)).max(1);
		}
		if img_px_h > 0 {
			height = height.min(img_px_h.saturating_sub(pan_y)).max(1);
		}

		KittyDisplay::DisplayImages(vec![KittyReadyToDisplay {
			img,
			page_num,
			pos: Position {
				x: img_area.x,
				y: img_area.y
			},
			display_loc: DisplayLocation {
				x: pan_x,
				y: pan_y,
				width,
				height,
				columns: img_area.width,
				rows: img_area.height,
				..DisplayLocation::default()
			}
		}])
	}

	#[must_use]
	pub fn render<'s>(
		&'s mut self,
		frame: &mut Frame<'_>,
		full_layout: &RenderLayout,
		font_size: FontSize
	) -> KittyDisplay<'s> {
		match &self.overlay {
			Overlay::Help => {
				self.render_help_msg(frame);
				return KittyDisplay::ClearImages;
			}
			Overlay::Index => {
				self.render_toc_overlay(frame);
				return KittyDisplay::ClearImages;
			}
			Overlay::Info => {
				self.render_info_overlay(frame);
				return KittyDisplay::ClearImages;
			}
			Overlay::VisualText(vs) => {
				Self::render_visual_overlay(frame, vs);
				return KittyDisplay::ClearImages;
			}
			Overlay::Picker(picker) => {
				Self::render_picker_overlay(frame, picker);
				return KittyDisplay::ClearImages;
			}
			Overlay::None => ()
		}

		if let Some(t_and_b) = full_layout.top_and_bottom {
			Self::render_top_and_bottom(
				t_and_b,
				self.page,
				self.state.page_offset,
				&self.page_constraints,
				&self.rendered,
				&self.name,
				frame,
				&self.bottom_msg,
				self.num_prefix,
				self.pending_prefix
			);
		}

		let mut img_area = full_layout.page_area;
		if img_area.width == 0 || img_area.height == 0 {
			return KittyDisplay::ClearImages;
		}

		let size = frame.area();
		if size == self.last_render.rect {
			frame.render_widget(Skip, img_area);
			return KittyDisplay::NoChange;
		}

		if let Some(ref mut zoom) = self.zoom
			&& self
				.rendered
				.get(self.page)
				.and_then(|slot| slot.img.as_ref())
				.is_some_and(|c| matches!(c, ConvertedImage::Kitty { .. }))
		{
			let Some(ConvertedImage::Kitty {
				ref mut img,
				cell_w,
				cell_h,
				px_w,
				px_h
			}) = self.rendered[self.page].img
			else {
				unreachable!()
			};

			self.last_render = LastRender {
				rect: size,
				pages_shown: 1,
				unused_width: 0
			};
			return Self::render_zoomed(
				img_area,
				font_size,
				zoom,
				img,
				self.page,
				(cell_w, cell_h),
				(px_w, px_h)
			);
		}

		if self.rendered.is_empty() || self.page >= self.rendered.len() {
			Self::render_loading_in(frame, img_area);
			return KittyDisplay::ClearImages;
		}

		let max_pages_this_view = if self.page_constraints.first_page_solo && self.page == 0 {
			Some(NonZeroUsize::MIN)
		} else {
			self.page_constraints.max_wide
		};

		let mut test_area_w = img_area.width;
		let mut page_sizes = self.rendered[self.page..]
			.iter_mut()
			.enumerate()
			.take_while(|(idx, page)| {
				let mut take = page.img.is_some();
				if let Some(max) = max_pages_this_view {
					take &= *idx < max.get();
				}
				take
			})
			.filter_map(|(_, page)| {
				page.img.as_mut().map(|img| {
					let (w, h) = img.w_h();
					(w, h, img)
				})
			})
			.take_while(|(width, height, _)| {
				*height <= img_area.height
					&& match test_area_w.checked_sub(*width) {
						Some(new_val) => {
							test_area_w = new_val;
							true
						}
						None => false
					}
			})
			.collect::<Vec<_>>();

		if self.page_constraints.r_to_l {
			page_sizes.reverse();
		}

		if page_sizes.is_empty() {
			Self::render_loading_in(frame, img_area);
			KittyDisplay::ClearImages
		} else {
			let _ = execute!(stdout(), BeginSynchronizedUpdate);

			let total_width = page_sizes.iter().map(|(w, _, _)| w).sum::<u16>();

			self.last_render.pages_shown = page_sizes.len();

			let unused_width = img_area.width - total_width;
			self.last_render.unused_width = unused_width;
			img_area.x += unused_width / 2;

			if let Some(total_height) = page_sizes.iter().map(|(_, h, _)| h).max()
				&& let Some(unused_height) = img_area.height.checked_sub(*total_height)
			{
				img_area.y += unused_height / 2;
			}

			let to_display = page_sizes
				.into_iter()
				.enumerate()
				.filter_map(|(idx, (width, _, img))| {
					let maybe_img =
						Self::render_single_page(frame, img, Rect { width, ..img_area });
					img_area.x += width;
					maybe_img.map(|(img, pos)| KittyReadyToDisplay {
						img,
						page_num: idx + self.page,
						pos,
						display_loc: DisplayLocation::default()
					})
				})
				.collect::<Vec<_>>();

			self.last_render.rect = size;

			KittyDisplay::DisplayImages(to_display)
		}
	}

	fn render_single_page<'img>(
		frame: &mut Frame<'_>,
		page_img: &'img mut ConvertedImage,
		img_area: Rect
	) -> Option<(&'img mut MaybeTransferred, Position)> {
		match page_img {
			ConvertedImage::Generic(page_img) => {
				frame.render_widget(Image::new(page_img), img_area);
				None
			}
			ConvertedImage::Kitty { img, .. } => Some((img, Position {
				x: img_area.x,
				y: img_area.y
			}))
		}
	}

	fn render_loading_in(frame: &mut Frame<'_>, area: Rect) {
		const LOADING_STR: &str = "Loading...";
		let inner_space =
			Layout::horizontal([Constraint::Length(const { LOADING_STR.len() as u16 })])
				.flex(Flex::Center)
				.split(area);

		let loading_span = Span::styled(LOADING_STR, Style::new().fg(Color::Cyan));

		frame.render_widget(loading_span, inner_space[0]);
	}

	fn change_page(
		&mut self,
		mut change: PageChange,
		amt: ChangeAmount,
		count: usize
	) -> Option<InputAction> {
		if self.rendered.is_empty() {
			return None;
		}
		let base_step = match amt {
			ChangeAmount::Single => 1,
			ChangeAmount::WholeScreen => self.last_render.pages_shown.max(1)
		};
		let diff = base_step.saturating_mul(count.max(1));

		if self.page_constraints.r_to_l {
			change = match change {
				PageChange::Next => PageChange::Prev,
				PageChange::Prev => PageChange::Next
			};
		}

		let old = self.page;
		match change {
			PageChange::Next => {
				self.set_page((self.page + diff).min(self.rendered.len().saturating_sub(1)));
				if self.page != old
					&& let Some(zoom) = &mut self.zoom
				{
					zoom.cell_pan_from_top = 0;
				}
			}
			PageChange::Prev => {
				self.set_page(self.page.saturating_sub(diff));
				if self.page != old
					&& let Some(zoom) = &mut self.zoom
				{
					zoom.cell_pan_from_top = u16::MAX;
				}
			}
		}

		self.bisect.reset();

		match self.page as isize - old as isize {
			0 => None,
			_ => {
				self.save_state();
				Some(InputAction::JumpingToPage(self.page))
			}
		}
	}

	pub fn set_n_pages(&mut self, n_pages: usize) {
		fill_default(&mut self.rendered, n_pages);
		self.page = self.page.min(n_pages - 1);
	}

	pub fn set_doc_metadata(&mut self, outlines: Vec<TocNode>, info: Vec<(String, String)>) {
		self.toc = TocState::new(outlines);
		self.doc_info = info;
	}

	pub fn set_page_links(&mut self, page_num: usize, links: Vec<PageLink>) {
		if let Some(slot) = self.rendered.get_mut(page_num) {
			slot.links = links;
		}
	}

	pub fn got_page_text(&mut self, page_num: usize, text: String, open_visual: bool) {
		if open_visual {
			self.overlay = Overlay::VisualText(VisualTextState::new(page_num, &text));
			self.last_render.rect = Rect::default();
		} else {
			let trimmed = text.trim();
			if trimmed.is_empty() {
				self.set_msg(MessageSetting::Some(BottomMessage::Error(format!(
					"Page {} has no extractable text",
					page_num + 1
				))));
			} else {
				let _ = copy_to_clipboard(trimmed, is_tmux());
				self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
					"Copied page {} text ({} chars) to clipboard",
					page_num + 1,
					trimmed.chars().count()
				))));
			}
		}
	}

	pub fn page_ready(&mut self, img: ConvertedImage, page_num: usize, num_results: usize) {
		if page_num >= self.page && page_num <= self.page + self.last_render.pages_shown {
			self.last_render.rect = Rect::default();
		} else {
			let img_w = img.w_h().0;
			if img_w > 0 && img_w <= self.last_render.unused_width {
				let num_fit = self.last_render.unused_width / img_w;
				if page_num >= self.page && (self.page + num_fit as usize) >= page_num {
					self.last_render.rect = Rect::default();
				}
			}
		}

		if let Some(slot) = self.rendered.get_mut(page_num) {
			let prev_links = std::mem::take(&mut slot.links);
			*slot = RenderedInfo {
				img: Some(img),
				num_results: Some(num_results),
				links: prev_links
			};
		}
	}

	pub fn evict_far_page(&mut self) -> Option<usize> {
		const MAX_CACHED_PAGES: usize = 32;
		let cached_count = self.rendered.iter().filter(|r| r.img.is_some()).count();
		if cached_count <= MAX_CACHED_PAGES {
			return None;
		}
		let farthest = self
			.rendered
			.iter()
			.enumerate()
			.filter(|(_, r)| r.img.is_some())
			.max_by_key(|(idx, _)| idx.abs_diff(self.page))
			.map(|(idx, _)| idx)?;
		if let Some(slot) = self.rendered.get_mut(farthest) {
			slot.img = None;
		}
		Some(farthest)
	}

	pub fn page_failed_display(&mut self, page_num: usize) {
		if let Some(slot) = self.rendered.get_mut(page_num) {
			slot.img = None;
		}
	}

	pub fn got_num_results_on_page(&mut self, page_num: usize, num_results: usize) {
		if let Some(slot) = self.rendered.get_mut(page_num) {
			slot.num_results = Some(num_results);
		}
	}

	#[expect(clippy::too_many_arguments)]
	fn render_top_and_bottom(
		(top_area, bottom_area): (Rect, Rect),
		page_num: usize,
		page_offset: i32,
		constraints: &PageConstraints,
		rendered: &[RenderedInfo],
		doc_name: &str,
		frame: &mut Frame<'_>,
		bottom_msg: &BottomMessage,
		num_prefix: Option<usize>,
		pending_prefix: PendingPrefix
	) {
		let total = rendered.len();
		let pct = if total > 0 {
			((page_num + 1) * 100) / total
		} else {
			0
		};
		let mode_tag = if constraints.max_wide.is_some_and(|w| w.get() >= 2) {
			if constraints.first_page_solo {
				"[2p:1] "
			} else {
				"[2p] "
			}
		} else {
			""
		};
		let prefix_str = match (num_prefix, pending_prefix) {
			(Some(n), PendingPrefix::G) => format!("{n}g "),
			(Some(n), PendingPrefix::Z) => format!("{n}z "),
			(Some(n), PendingPrefix::None) => format!("{n} "),
			(None, PendingPrefix::G) => "g ".to_string(),
			(None, PendingPrefix::Z) => "z ".to_string(),
			(None, PendingPrefix::None) => String::new()
		};
		let page_nums_text = if page_offset != 0 {
			let logical = (page_num as i32 + 1) - page_offset;
			format!(
				"{prefix_str}{mode_tag}{logical} ({}/{total}) [{pct}%] ",
				page_num + 1
			)
		} else {
			format!("{prefix_str}{mode_tag}{} / {total} [{pct}%] ", page_num + 1)
		};

		let top_block = Block::new()
			.title_top(" ")
			.title_top(Span::styled(doc_name, Style::new().fg(Color::Cyan)))
			.title_top(
				Span::styled(&page_nums_text, Style::new().fg(Color::Cyan))
					.into_right_aligned_line()
			)
			.padding(Padding {
				bottom: 1,
				..Padding::default()
			})
			.borders(Borders::BOTTOM);

		frame.render_widget(top_block, top_area);

		let bottom_block = Block::new()
			.padding(Padding {
				top: 1,
				right: 2,
				left: 2,
				bottom: 0
			})
			.borders(Borders::TOP);
		let bottom_inside_block = bottom_block.inner(bottom_area);

		frame.render_widget(bottom_block, bottom_area);

		let rendered_str = if !rendered.is_empty() {
			format!(
				"Rendered: {}%",
				(rendered.iter().filter(|i| i.img.is_some()).count() * 100) / rendered.len()
			)
		} else {
			String::new()
		};
		let bottom_layout = Layout::horizontal([
			Constraint::Fill(1),
			Constraint::Length(rendered_str.len() as u16)
		])
		.split(bottom_inside_block);

		let rendered_span = Span::styled(&rendered_str, Style::new().fg(Color::Cyan));
		frame.render_widget(rendered_span, bottom_layout[1]);

		let (msg_str, color): (Cow<'_, str>, _) = match bottom_msg {
			BottomMessage::Help => (
				":help / F1: Help | Tab: Index | /: Search | :: Command".into(),
				Color::Blue
			),
			BottomMessage::Info(msg) => (msg.as_str().into(), Color::Cyan),
			BottomMessage::Error(e) => (e.as_str().into(), Color::Red),
			BottomMessage::Input(input_state) => (
				match input_state {
					InputCommand::Search {
						term,
						backward: false
					} => format!("/{term}"),
					InputCommand::Search {
						term,
						backward: true
					} => format!("?{term}"),
					InputCommand::Command(s) => format!(":{s}"),
					InputCommand::MarkSet => "m".to_string(),
					InputCommand::MarkJump => "'".to_string()
				}
				.into(),
				Color::Blue
			),
			BottomMessage::SearchResults(term) => {
				let num_found = rendered.iter().filter_map(|r| r.num_results).sum::<usize>();
				let pages_hit = rendered
					.iter()
					.filter(|r| r.num_results.is_some_and(|n| n > 0))
					.count();
				let num_searched =
					rendered.iter().filter(|r| r.num_results.is_some()).count() * 100;
				let pct_searched = if rendered.is_empty() {
					0
				} else {
					num_searched / rendered.len()
				};
				(
					format!(
						"/{term}: {num_found} hits on {pages_hit}p (searched: {pct_searched}%)"
					)
					.into(),
					Color::Blue
				)
			}
			BottomMessage::Reloaded => ("Document was reloaded!".into(), Color::Blue)
		};

		let span = Span::styled(msg_str, Style::new().fg(color));
		frame.render_widget(span, bottom_layout[0]);
	}

	fn handle_overlay_key(
		&mut self,
		code: KeyCode,
		modifiers: KeyModifiers
	) -> Option<InputAction> {
		let ctrl = modifiers.contains(KeyModifiers::CONTROL);
		let is_abort = matches!(code, KeyCode::Esc)
			|| (ctrl && matches!(code, KeyCode::Char('c' | '[')));

		match &mut self.overlay {
			Overlay::None => None,
			Overlay::Help | Overlay::Info => {
				if is_abort || matches!(code, KeyCode::Char('q') | KeyCode::F(1)) {
					self.overlay = Overlay::None;
					self.last_render.rect = Rect::default();
					Some(InputAction::Redraw)
				} else {
					None
				}
			}
			Overlay::Index => {
				let total = self.toc.flat.len();
				if is_abort || matches!(code, KeyCode::Tab) {
					self.toc.pending_g = false;
					self.toc.pending_z = false;
					self.overlay = Overlay::None;
					self.last_render.rect = Rect::default();
					return Some(InputAction::Redraw);
				}
				match code {
					KeyCode::Char('q') => Some(InputAction::QuitApp),
					KeyCode::Char('d') if ctrl => {
						if total > 0 {
							self.toc.cursor = (self.toc.cursor + 10).min(total - 1);
						}
						Some(InputAction::Redraw)
					}
					KeyCode::Char('u') if ctrl => {
						self.toc.cursor = self.toc.cursor.saturating_sub(10);
						Some(InputAction::Redraw)
					}
					KeyCode::Char('f') if ctrl => {
						if total > 0 {
							self.toc.cursor = (self.toc.cursor + 20).min(total - 1);
						}
						Some(InputAction::Redraw)
					}
					KeyCode::Char('b') if ctrl => {
						self.toc.cursor = self.toc.cursor.saturating_sub(20);
						Some(InputAction::Redraw)
					}
					KeyCode::Char('j') if ctrl => self.activate_selected_toc_entry(),
					KeyCode::Char(c) if self.toc.pending_z => {
						self.toc.pending_z = false;
						match c {
							'c' => self.toc.set_current_expanded(false, false),
							'C' => self.toc.set_current_expanded(false, true),
							'M' => self.toc.set_all_expanded(false),
							'o' => self.toc.set_current_expanded(true, false),
							'O' => self.toc.set_current_expanded(true, true),
							'R' => self.toc.set_all_expanded(true),
							_ => ()
						}
						Some(InputAction::Redraw)
					}
					KeyCode::Char('g') if self.toc.pending_g => {
						self.toc.pending_g = false;
						self.toc.cursor = 0;
						Some(InputAction::Redraw)
					}
					KeyCode::Char('g') => {
						self.toc.pending_g = true;
						None
					}
					KeyCode::Char('z') => {
						self.toc.pending_z = true;
						None
					}
					KeyCode::Char('j') | KeyCode::Down => {
						self.toc.pending_g = false;
						if total > 0 {
							self.toc.cursor = (self.toc.cursor + 1).min(total - 1);
						}
						Some(InputAction::Redraw)
					}
					KeyCode::Char('k') | KeyCode::Up => {
						self.toc.pending_g = false;
						self.toc.cursor = self.toc.cursor.saturating_sub(1);
						Some(InputAction::Redraw)
					}
					KeyCode::PageDown => {
						if total > 0 {
							self.toc.cursor = (self.toc.cursor + 15).min(total - 1);
						}
						Some(InputAction::Redraw)
					}
					KeyCode::PageUp => {
						self.toc.cursor = self.toc.cursor.saturating_sub(15);
						Some(InputAction::Redraw)
					}
					KeyCode::Char('G') | KeyCode::End => {
						self.toc.pending_g = false;
						if total > 0 {
							self.toc.cursor = total - 1;
						}
						Some(InputAction::Redraw)
					}
					KeyCode::Home => {
						self.toc.cursor = 0;
						Some(InputAction::Redraw)
					}
					KeyCode::Char('h') | KeyCode::Left => {
						self.toc.set_current_expanded(false, false);
						Some(InputAction::Redraw)
					}
					KeyCode::Char('H') => {
						self.toc.set_all_expanded(false);
						Some(InputAction::Redraw)
					}
					KeyCode::Char('l') | KeyCode::Right => {
						self.toc.set_current_expanded(true, false);
						Some(InputAction::Redraw)
					}
					KeyCode::Char('L') => {
						self.toc.set_all_expanded(true);
						Some(InputAction::Redraw)
					}
					KeyCode::Enter | KeyCode::Char(' ') => self.activate_selected_toc_entry(),
					_ => {
						self.toc.pending_g = false;
						self.toc.pending_z = false;
						None
					}
				}
			}
			Overlay::VisualText(vs) => {
				let total = vs.lines.len();
				if is_abort {
					self.overlay = Overlay::None;
					self.last_render.rect = Rect::default();
					return Some(InputAction::Redraw);
				}
				match code {
					KeyCode::Char('q') => Some(InputAction::QuitApp),
					KeyCode::Char('d') if ctrl => {
						vs.cursor = (vs.cursor + 10).min(total.saturating_sub(1));
						Some(InputAction::Redraw)
					}
					KeyCode::Char('u') if ctrl => {
						vs.cursor = vs.cursor.saturating_sub(10);
						Some(InputAction::Redraw)
					}
					KeyCode::Char('g') if vs.pending_g => {
						vs.pending_g = false;
						vs.cursor = 0;
						Some(InputAction::Redraw)
					}
					KeyCode::Char('g') => {
						vs.pending_g = true;
						None
					}
					KeyCode::Char('G') | KeyCode::End => {
						vs.pending_g = false;
						vs.cursor = total.saturating_sub(1);
						Some(InputAction::Redraw)
					}
					KeyCode::Home | KeyCode::Char('0' | '^') => {
						vs.cursor = 0;
						Some(InputAction::Redraw)
					}
					KeyCode::Char('$') => {
						vs.cursor = total.saturating_sub(1);
						Some(InputAction::Redraw)
					}
					KeyCode::Char('j') | KeyCode::Down => {
						vs.pending_g = false;
						vs.cursor = (vs.cursor + 1).min(total.saturating_sub(1));
						Some(InputAction::Redraw)
					}
					KeyCode::Char('k') | KeyCode::Up => {
						vs.pending_g = false;
						vs.cursor = vs.cursor.saturating_sub(1);
						Some(InputAction::Redraw)
					}
					KeyCode::Char('v' | 'V') => {
						vs.pending_g = false;
						if vs.anchor.is_some() {
							vs.anchor = None;
						} else {
							vs.anchor = Some(vs.cursor);
						}
						Some(InputAction::Redraw)
					}
					KeyCode::Char('o') => {
						if let Some(ref mut a) = vs.anchor {
							std::mem::swap(a, &mut vs.cursor);
						}
						Some(InputAction::Redraw)
					}
					KeyCode::Char(':' ) => {
						self.overlay = Overlay::None;
						self.last_render.rect = Rect::default();
						self.set_msg(MessageSetting::Some(BottomMessage::Input(
							InputCommand::Command(String::new())
						)));
						Some(InputAction::Redraw)
					}
					KeyCode::Char('y' | 'Y') => {
						let selected = vs.selected_text();
						let (start, end) = vs.selected_range();
						let line_count = end - start + 1;
						let _ = copy_to_clipboard(&selected, is_tmux());
						self.overlay = Overlay::None;
						self.last_render.rect = Rect::default();
						self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
							"Yanked {line_count} line(s) to clipboard"
						))));
						Some(InputAction::Redraw)
					}
					_ => {
						vs.pending_g = false;
						None
					}
				}
			}
			Overlay::Picker(picker) => {
				let total = picker.items.len();
				if is_abort {
					self.overlay = Overlay::None;
					self.last_render.rect = Rect::default();
					return Some(InputAction::Redraw);
				}
				match code {
					KeyCode::Char('q') => Some(InputAction::QuitApp),
					KeyCode::Char('j') | KeyCode::Down => {
						if total > 0 {
							picker.cursor = (picker.cursor + 1).min(total - 1);
						}
						Some(InputAction::Redraw)
					}
					KeyCode::Char('k') | KeyCode::Up => {
						picker.cursor = picker.cursor.saturating_sub(1);
						Some(InputAction::Redraw)
					}
					KeyCode::Char('G') | KeyCode::End => {
						if total > 0 {
							picker.cursor = total - 1;
						}
						Some(InputAction::Redraw)
					}
					KeyCode::Home => {
						picker.cursor = 0;
						Some(InputAction::Redraw)
					}
					KeyCode::Char(c @ '1'..='9') => {
						let idx = (c as usize) - ('1' as usize);
						if idx < total {
							picker.cursor = idx;
							return self.activate_picker_selection();
						}
						None
					}
					KeyCode::Enter | KeyCode::Char(' ') => self.activate_picker_selection(),
					_ => None
				}
			}
		}
	}

	fn activate_selected_toc_entry(&mut self) -> Option<InputAction> {
		let item = self.toc.flat.get(self.toc.cursor).cloned()?;
		self.overlay = Overlay::None;
		self.last_render.rect = Rect::default();
		if let Some(target_page) = item.page {
			return self.jump_with_history(MarkPos {
				page: target_page,
				pan_left: 0,
				pan_top: 0
			});
		}
		if let Some(uri) = item.uri {
			let _ = open_external_uri(&uri);
			self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
				"Opened URI: {uri}"
			))));
		}
		Some(InputAction::Redraw)
	}

	fn activate_picker_selection(&mut self) -> Option<InputAction> {
		let Overlay::Picker(ref picker) = self.overlay else {
			return None;
		};
		let item = picker.items.get(picker.cursor).cloned()?;
		let kind = picker.kind.clone();
		self.overlay = Overlay::None;
		self.last_render.rect = Rect::default();

		match kind {
			PickerKind::Bookmarks => {
				if let Some(page) = item.target_page {
					return self.jump_with_history(MarkPos {
						page,
						pan_left: 0,
						pan_top: 0
					});
				}
				Some(InputAction::Redraw)
			}
			PickerKind::JumpList => {
				if let Some(pos) = item.mark_pos {
					return self.apply_mark_pos(pos);
				}
				Some(InputAction::Redraw)
			}
			PickerKind::Links(action) => self.execute_link_action(
				action,
				item.target_page,
				item.uri.as_deref().unwrap_or_default()
			)
		}
	}

	fn execute_link_action(
		&mut self,
		action: LinkActionKind,
		dest_page: Option<usize>,
		uri: &str
	) -> Option<InputAction> {
		match action {
			LinkActionKind::Follow => {
				if let Some(page) = dest_page {
					return self.jump_with_history(MarkPos {
						page,
						pan_left: 0,
						pan_top: 0
					});
				}
				if !uri.is_empty() {
					if let Err(e) = open_external_uri(uri) {
						self.set_msg(MessageSetting::Some(BottomMessage::Error(e)));
					} else {
						self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
							"Opened link: {uri}"
						))));
					}
				}
				Some(InputAction::Redraw)
			}
			LinkActionKind::Display => {
				let desc = match (dest_page, uri.is_empty()) {
					(Some(p), false) => format!("Link -> Page {} ({uri})", p + 1),
					(Some(p), true) => format!("Link -> Page {}", p + 1),
					(None, _) => format!("Link -> {uri}")
				};
				self.set_msg(MessageSetting::Some(BottomMessage::Info(desc)));
				Some(InputAction::Redraw)
			}
			LinkActionKind::Copy => {
				let text = if !uri.is_empty() {
					uri.to_string()
				} else if let Some(p) = dest_page {
					format!("page:{}", p + 1)
				} else {
					String::new()
				};
				let _ = copy_to_clipboard(&text, is_tmux());
				self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
					"Copied link: {text}"
				))));
				Some(InputAction::Redraw)
			}
		}
	}

	fn trigger_link_mode(&mut self, action: LinkActionKind) -> Option<InputAction> {
		let shown = self.last_render.pages_shown.max(1);
		let mut visible_links = Vec::new();
		for p_idx in self.page..(self.page + shown).min(self.rendered.len()) {
			for link in &self.rendered[p_idx].links {
				visible_links.push(link.clone());
			}
		}

		if visible_links.is_empty() {
			self.set_msg(MessageSetting::Some(BottomMessage::Info(
				"No links on current page".to_string()
			)));
			return Some(InputAction::Redraw);
		}

		if visible_links.len() == 1 {
			let l = &visible_links[0];
			return self.execute_link_action(action, l.dest_page, &l.uri);
		}

		let title = match action {
			LinkActionKind::Follow => "Follow Link (1-9 or Enter)",
			LinkActionKind::Display => "Display Link Target (1-9 or Enter)",
			LinkActionKind::Copy => "Copy Link Target (1-9 or Enter)"
		}
		.to_string();

		let items = visible_links
			.into_iter()
			.enumerate()
			.map(|(idx, l)| {
				let target_desc = match (l.dest_page, l.uri.is_empty()) {
					(Some(p), _) => format!("Page {}", p + 1),
					(None, false) => l.uri.clone(),
					(None, true) => "(unknown)".to_string()
				};
				PickerItem {
					label: format!("[{}] (p.{}) -> {}", idx + 1, l.page + 1, target_desc),
					target_page: l.dest_page,
					uri: Some(l.uri),
					mark_pos: None
				}
			})
			.collect();

		self.overlay = Overlay::Picker(ListPickerState {
			title,
			kind: PickerKind::Links(action),
			items,
			cursor: 0
		});
		self.last_render.rect = Rect::default();
		Some(InputAction::Redraw)
	}

	fn logical_to_zero_page(&self, logical_input: usize) -> Option<usize> {
		let phys_one_based = (logical_input as i32) + self.state.page_offset;
		if phys_one_based < 1 {
			return None;
		}
		let zero_page = (phys_one_based as usize) - 1;
		(zero_page < self.rendered.len()).then_some(zero_page)
	}

	fn search_step(&mut self, forward: bool) -> Option<InputAction> {
		if self.rendered.is_empty() {
			return None;
		}
		if forward {
			if self.page >= self.rendered.len() - 1 {
				return None;
			}
			self.rendered[(self.page + 1)..]
				.iter()
				.enumerate()
				.find_map(|(idx, p)| {
					p.num_results
						.is_some_and(|num| num > 0)
						.then_some(self.page + 1 + idx)
				})
				.and_then(|next_page| {
					self.jump_with_history(MarkPos {
						page: next_page,
						pan_left: 0,
						pan_top: 0
					})
				})
		} else {
			if self.page == 0 {
				return None;
			}
			self.rendered[..self.page]
				.iter()
				.rev()
				.enumerate()
				.find_map(|(idx, p)| {
					p.num_results
						.is_some_and(|num| num > 0)
						.then_some(self.page - (idx + 1))
				})
				.and_then(|prev_page| {
					self.jump_with_history(MarkPos {
						page: prev_page,
						pan_left: 0,
						pan_top: 0
					})
				})
		}
	}

	fn execute_colon_command(&mut self, raw: &str) -> Option<InputAction> {
		let trimmed = raw.trim();
		if trimmed.is_empty() {
			return Some(InputAction::Redraw);
		}
		if let Ok(page_num) = trimmed.parse::<usize>() {
			if let Some(zero_page) = self.logical_to_zero_page(page_num) {
				return self.jump_with_history(MarkPos {
					page: zero_page,
					pan_left: 0,
					pan_top: 0
				});
			}
			self.set_msg(MessageSetting::Some(BottomMessage::Error(format!(
				"Page {page_num} is out of range"
			))));
			return Some(InputAction::Redraw);
		}

		let (cmd, arg) = trimmed
			.split_once(char::is_whitespace)
			.map_or((trimmed, ""), |(c, a)| (c, a.trim()));

		match cmd {
			"q" | "quit" | "close" => Some(InputAction::QuitApp),
			"help" | "h" => {
				self.overlay = Overlay::Help;
				self.last_render.rect = Rect::default();
				Some(InputAction::Redraw)
			}
			"info" => {
				self.overlay = Overlay::Info;
				self.last_render.rect = Rect::default();
				Some(InputAction::Redraw)
			}
			"reload" | "e" => Some(InputAction::Reload),
			"yank" | "y" => Some(InputAction::ExtractPageText {
				page: self.page,
				open_visual: false
			}),
			"nohlsearch" | "nohl" => {
				for img in &mut self.rendered {
					img.num_results = None;
				}
				self.set_msg(MessageSetting::Reset);
				Some(InputAction::Search(String::new()))
			}
			"hlsearch" => {
				if self.last_search_term.is_empty() {
					self.set_msg(MessageSetting::Some(BottomMessage::Info(
						"No previous search term".to_string()
					)));
					Some(InputAction::Redraw)
				} else {
					let term = self.last_search_term.clone();
					for img in &mut self.rendered {
						img.num_results = None;
					}
					self.set_msg(MessageSetting::Some(BottomMessage::SearchResults(
						term.clone()
					)));
					Some(InputAction::Search(term))
				}
			}
			"offset" => {
				if arg.is_empty() {
					self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
						"Current page offset: {}",
						self.state.page_offset
					))));
				} else if let Ok(off) = arg.parse::<i32>() {
					self.state.page_offset = off;
					self.save_state();
					self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
						"Page offset set to {off}"
					))));
				} else {
					self.set_msg(MessageSetting::Some(BottomMessage::Error(format!(
						"Invalid integer offset: '{arg}'"
					))));
				}
				Some(InputAction::Redraw)
			}
			"bmark" => {
				if arg.is_empty() {
					self.set_msg(MessageSetting::Some(BottomMessage::Error(
						"Usage: :bmark <name>".to_string()
					)));
				} else {
					self.state.set_bookmark(arg.to_string(), self.page);
					self.save_state();
					self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
						"Bookmark '{arg}' saved at page {}",
						self.page + 1
					))));
				}
				Some(InputAction::Redraw)
			}
			"bdelete" | "bdel" => {
				if arg.is_empty() {
					self.set_msg(MessageSetting::Some(BottomMessage::Error(
						"Usage: :bdelete <name>".to_string()
					)));
				} else if self.state.delete_bookmark(arg) {
					self.save_state();
					self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
						"Deleted bookmark '{arg}'"
					))));
				} else {
					self.set_msg(MessageSetting::Some(BottomMessage::Error(format!(
						"Bookmark '{arg}' not found"
					))));
				}
				Some(InputAction::Redraw)
			}
			"bjump" => {
				if let Some(target_page) = self.state.get_bookmark(arg) {
					return self.jump_with_history(MarkPos {
						page: target_page,
						pan_left: 0,
						pan_top: 0
					});
				}
				self.set_msg(MessageSetting::Some(BottomMessage::Error(format!(
					"Bookmark '{arg}' not found"
				))));
				Some(InputAction::Redraw)
			}
			"blist" | "bookmarks" => {
				if self.state.bookmarks.is_empty() {
					self.set_msg(MessageSetting::Some(BottomMessage::Info(
						"No bookmarks saved for this document".to_string()
					)));
					return Some(InputAction::Redraw);
				}
				let items = self
					.state
					.bookmarks
					.iter()
					.enumerate()
					.map(|(idx, (name, p))| PickerItem {
						label: format!("[{}] {name} -> Page {}", idx + 1, p + 1),
						target_page: Some(*p),
						uri: None,
						mark_pos: None
					})
					.collect();
				self.overlay = Overlay::Picker(ListPickerState {
					title: "Bookmarks (Enter to jump)".to_string(),
					kind: PickerKind::Bookmarks,
					items,
					cursor: 0
				});
				self.last_render.rect = Rect::default();
				Some(InputAction::Redraw)
			}
			"jumplist" | "jumps" => {
				let entries = self.jumplist.entries();
				if entries.is_empty() {
					self.set_msg(MessageSetting::Some(BottomMessage::Info(
						"Jumplist is empty".to_string()
					)));
					return Some(InputAction::Redraw);
				}
				let items = entries
					.iter()
					.enumerate()
					.rev()
					.map(|(idx, pos)| PickerItem {
						label: format!(
							"#{}: Page {} (pan {},{})",
							idx + 1,
							pos.page + 1,
							pos.pan_left,
							pos.pan_top
						),
						target_page: Some(pos.page),
						uri: None,
						mark_pos: Some(*pos)
					})
					.collect();
				self.overlay = Overlay::Picker(ListPickerState {
					title: "Jumplist (most recent first)".to_string(),
					kind: PickerKind::JumpList,
					items,
					cursor: 0
				});
				self.last_render.rect = Rect::default();
				Some(InputAction::Redraw)
			}
			"mark" => {
				if let Some(ch) = arg.chars().next()
					&& ch.is_ascii_alphanumeric()
				{
					let pos = self.current_mark_pos();
					self.state.set_mark(ch, pos);
					self.save_state();
					self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
						"Mark '{ch}' set at page {}",
						self.page + 1
					))));
				} else {
					self.set_msg(MessageSetting::Some(BottomMessage::Error(
						"Usage: :mark <a-zA-Z0-9>".to_string()
					)));
				}
				Some(InputAction::Redraw)
			}
			"delmarks" | "delm" => {
				if arg.is_empty() {
					self.set_msg(MessageSetting::Some(BottomMessage::Error(
						"Usage: :delmarks <chars> or :delmarks !".to_string()
					)));
				} else {
					let removed = self.state.delete_marks(arg);
					self.save_state();
					self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
						"Deleted {removed} mark(s)"
					))));
				}
				Some(InputAction::Redraw)
			}
			_ => {
				self.set_msg(MessageSetting::Some(BottomMessage::Error(format!(
					"Unknown command: :{trimmed}"
				))));
				Some(InputAction::Redraw)
			}
		}
	}

	fn zoom_in(&mut self) -> Option<InputAction> {
		if !self.is_kitty {
			return None;
		}
		if self.zoom.is_some() {
			self.update_zoom(Zoom::step_in)
		} else {
			self.zoom = Some(Zoom {
				level: 1,
				..Zoom::default()
			});
			self.last_render.rect = Rect::default();
			self.save_state();
			Some(InputAction::SwitchRenderZoom(FitOrFill::Fill))
		}
	}

	fn zoom_out(&mut self) -> Option<InputAction> {
		if !self.is_kitty {
			return None;
		}
		if self.zoom.is_some() {
			self.update_zoom(Zoom::step_out)
		} else {
			None
		}
	}

	fn zoom_original(&mut self) -> Option<InputAction> {
		if !self.is_kitty {
			return None;
		}
		if self.zoom.is_some() {
			self.update_zoom(Zoom::reset_level)
		} else {
			self.zoom = Some(Zoom::default());
			self.last_render.rect = Rect::default();
			self.save_state();
			Some(InputAction::SwitchRenderZoom(FitOrFill::Fill))
		}
	}

	pub fn handle_event(&mut self, ev: &Event) -> Option<InputAction> {
		let can_zoom = self.is_kitty && self.zoom.is_some();

		match ev {
			Event::Key(key) => {
				if !matches!(self.overlay, Overlay::None) {
					return self.handle_overlay_key(key.code, key.modifiers);
				}

				let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
				let alt = key.modifiers.contains(KeyModifiers::ALT);
				let shift = key.modifiers.contains(KeyModifiers::SHIFT);
				let is_abort = matches!(key.code, KeyCode::Esc)
					|| (ctrl && matches!(key.code, KeyCode::Char('c' | '[')));

				if is_abort {
					self.num_prefix = None;
					self.pending_prefix = PendingPrefix::None;
					if matches!(self.bottom_msg, BottomMessage::Input(_)) {
						self.set_msg(MessageSetting::Pop);
						return Some(InputAction::Redraw);
					}
					if matches!(self.bottom_msg, BottomMessage::SearchResults(_)) {
						for img in &mut self.rendered {
							img.num_results = None;
						}
						self.set_msg(MessageSetting::Reset);
						return Some(InputAction::Search(String::new()));
					}
					self.set_msg(MessageSetting::Reset);
					return Some(InputAction::Redraw);
				}

				if matches!(self.bottom_msg, BottomMessage::Input(_)) {
					return self.handle_inputbar_key(key.code, ctrl);
				}

				if self.pending_prefix != PendingPrefix::None {
					let pending = self.pending_prefix;
					self.pending_prefix = PendingPrefix::None;
					let count = self.num_prefix.take();
					return match (pending, key.code) {
						(PendingPrefix::G, KeyCode::Char('g')) => {
							if let Some(target_n) = count {
								if let Some(zero_page) = self.logical_to_zero_page(target_n) {
									self.jump_with_history(MarkPos {
										page: zero_page,
										pan_left: 0,
										pan_top: 0
									})
								} else {
									self.set_msg(MessageSetting::Some(BottomMessage::Error(
										format!("Page {target_n} is out of range")
									)));
									Some(InputAction::Redraw)
								}
							} else {
								self.jump_with_history(MarkPos {
									page: 0,
									pan_left: 0,
									pan_top: 0
								})
							}
						}
						(PendingPrefix::G, KeyCode::Char('v')) => Some(
							InputAction::ExtractPageText {
								page: self.page,
								open_visual: true
							}
						),
						(PendingPrefix::Z, KeyCode::Char('i' | 'I')) => self.zoom_in(),
						(PendingPrefix::Z, KeyCode::Char('o' | 'O')) => self.zoom_out(),
						(PendingPrefix::Z, KeyCode::Char('0' | 'z' | 'Z')) => self.zoom_original(),
						_ => Some(InputAction::Redraw)
					};
				}

				if !ctrl
					&& !alt
					&& let KeyCode::Char(c) = key.code
					&& let Some(digit) = c.to_digit(10)
					&& (digit > 0 || self.num_prefix.is_some())
				{
					let cur = self.num_prefix.unwrap_or(0);
					self.num_prefix = Some(cur.saturating_mul(10).saturating_add(digit as usize));
					return Some(InputAction::Redraw);
				}

				let count = self.num_prefix.take();

				match key.code {
					KeyCode::Tab => {
						if self.toc.roots.is_empty() {
							self.set_msg(MessageSetting::Some(BottomMessage::Info(
								"Document has no Table of Contents (outline)".to_string()
							)));
						} else {
							self.toc.sync_to_page(self.page);
							self.overlay = Overlay::Index;
							self.last_render.rect = Rect::default();
						}
						Some(InputAction::Redraw)
					}
					KeyCode::F(1) => {
						self.overlay = Overlay::Help;
						self.last_render.rect = Rect::default();
						Some(InputAction::Redraw)
					}
					KeyCode::F(5 | 11) => Some(InputAction::Fullscreen),
					KeyCode::Char('n') if ctrl => Some(InputAction::Fullscreen),
					KeyCode::Char('o') if ctrl => {
						let cur = self.current_mark_pos();
						if let Some(prev) = self.jumplist.go_back(cur) {
							self.apply_mark_pos(prev)
						} else {
							self.set_msg(MessageSetting::Some(BottomMessage::Info(
								"At oldest jump in jumplist".to_string()
							)));
							Some(InputAction::Redraw)
						}
					}
					KeyCode::Char('i') if ctrl => {
						let cur = self.current_mark_pos();
						if let Some(next) = self.jumplist.go_forward(cur) {
							self.apply_mark_pos(next)
						} else {
							self.set_msg(MessageSetting::Some(BottomMessage::Info(
								"At newest jump in jumplist".to_string()
							)));
							Some(InputAction::Redraw)
						}
					}
					KeyCode::Char('j') if ctrl => {
						let total = self.rendered.len();
						if let Some((target, low, high)) =
							self.bisect.step(self.page, total, true)
						{
							let cur = self.current_mark_pos();
							self.jumplist.record(cur);
							self.set_page(target);
							self.save_state();
							self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
								"Bisect -> page {} (range {}..{})",
								target + 1,
								low + 1,
								high + 1
							))));
							Some(InputAction::JumpingToPage(target))
						} else {
							None
						}
					}
					KeyCode::Char('k') if ctrl => {
						let total = self.rendered.len();
						if let Some((target, low, high)) =
							self.bisect.step(self.page, total, false)
						{
							let cur = self.current_mark_pos();
							self.jumplist.record(cur);
							self.set_page(target);
							self.save_state();
							self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
								"Bisect -> page {} (range {}..{})",
								target + 1,
								low + 1,
								high + 1
							))));
							Some(InputAction::JumpingToPage(target))
						} else {
							None
						}
					}
					KeyCode::Char('d') if ctrl => {
						if can_zoom {
							if self
								.zoom
								.is_some_and(|z| z.cell_pan_from_top >= z.max_pan_top)
							{
								self.change_page(PageChange::Next, ChangeAmount::Single, 1)
							} else {
								self.update_zoom(|z| z.pan_by(Direction::Down, Zoom::PAN_STEP_Y * 2))
							}
						} else {
							self.change_page(
								PageChange::Next,
								ChangeAmount::Single,
								count.unwrap_or(1)
							)
						}
					}
					KeyCode::Char('u') if ctrl => {
						if can_zoom {
							if self.zoom.is_some_and(|z| z.cell_pan_from_top == 0) {
								self.change_page(PageChange::Prev, ChangeAmount::Single, 1)
							} else {
								self.update_zoom(|z| z.pan_by(Direction::Up, Zoom::PAN_STEP_Y * 2))
							}
						} else {
							self.change_page(
								PageChange::Prev,
								ChangeAmount::Single,
								count.unwrap_or(1)
							)
						}
					}
					KeyCode::Char('f') if ctrl => self.change_page(
						PageChange::Next,
						ChangeAmount::WholeScreen,
						count.unwrap_or(1)
					),
					KeyCode::Char('b') if ctrl => self.change_page(
						PageChange::Prev,
						ChangeAmount::WholeScreen,
						count.unwrap_or(1)
					),
					KeyCode::Char('t') if ctrl => {
						if can_zoom {
							self.update_zoom(|z| z.pan_by(Direction::Left, Zoom::PAN_STEP_X * 2))
						} else {
							None
						}
					}
					KeyCode::Char('y') if ctrl => {
						if can_zoom {
							self.update_zoom(|z| z.pan_by(Direction::Right, Zoom::PAN_STEP_X * 2))
						} else {
							None
						}
					}
					KeyCode::Char('r') if ctrl => {
						self.state.invert = !self.state.invert;
						self.save_state();
						Some(InputAction::Invert)
					}
					KeyCode::Char(c) => match c {
						'q' => Some(InputAction::QuitApp),
						'g' => {
							self.num_prefix = count;
							self.pending_prefix = PendingPrefix::G;
							Some(InputAction::Redraw)
						}
						'z' => {
							self.num_prefix = count;
							self.pending_prefix = PendingPrefix::Z;
							Some(InputAction::Redraw)
						}
						'G' => {
							if let Some(target_n) = count {
								if let Some(zero_page) = self.logical_to_zero_page(target_n) {
									self.jump_with_history(MarkPos {
										page: zero_page,
										pan_left: 0,
										pan_top: 0
									})
								} else {
									self.set_msg(MessageSetting::Some(BottomMessage::Error(
										format!("Page {target_n} is out of range")
									)));
									Some(InputAction::Redraw)
								}
							} else {
								let last = self.rendered.len().saturating_sub(1);
								self.jump_with_history(MarkPos {
									page: last,
									pan_left: 0,
									pan_top: 0
								})
							}
						}
						'J' => self.change_page(
							PageChange::Next,
							ChangeAmount::Single,
							count.unwrap_or(1)
						),
						'K' => self.change_page(
							PageChange::Prev,
							ChangeAmount::Single,
							count.unwrap_or(1)
						),
						' ' => {
							if shift {
								self.change_page(
									PageChange::Prev,
									ChangeAmount::WholeScreen,
									count.unwrap_or(1)
								)
							} else {
								self.change_page(
									PageChange::Next,
									ChangeAmount::WholeScreen,
									count.unwrap_or(1)
								)
							}
						}
						'H' => {
							if can_zoom {
								self.update_zoom(Zoom::pan_page_top)
							} else {
								None
							}
						}
						'L' => {
							if can_zoom {
								self.update_zoom(Zoom::pan_page_bottom)
							} else {
								None
							}
						}
						'P' => {
							if can_zoom {
								self.update_zoom(|z| {
									z.pan_page_top();
									z.pan_left();
								})
							} else {
								None
							}
						}
						't' => {
							if can_zoom {
								self.update_zoom(Zoom::pan_left)
							} else {
								None
							}
						}
						'y' => {
							if can_zoom {
								self.update_zoom(Zoom::pan_right)
							} else {
								Some(InputAction::ExtractPageText {
									page: self.page,
									open_visual: false
								})
							}
						}
						'Y' => {
							let path_str = self.file_path.to_string_lossy().to_string();
							let _ = copy_to_clipboard(&path_str, is_tmux());
							self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
								"Copied filepath: {path_str}"
							))));
							Some(InputAction::Redraw)
						}
						':' => {
							self.set_msg(MessageSetting::Some(BottomMessage::Input(
								InputCommand::Command(String::new())
							)));
							Some(InputAction::Redraw)
						}
						'm' => {
							self.set_msg(MessageSetting::Some(BottomMessage::Input(
								InputCommand::MarkSet
							)));
							Some(InputAction::Redraw)
						}
						'\'' => {
							self.set_msg(MessageSetting::Some(BottomMessage::Input(
								InputCommand::MarkJump
							)));
							Some(InputAction::Redraw)
						}
						'/' => {
							self.set_msg(MessageSetting::Some(BottomMessage::Input(
								InputCommand::Search {
									term: String::new(),
									backward: false
								}
							)));
							Some(InputAction::Redraw)
						}
						'?' => {
							self.set_msg(MessageSetting::Some(BottomMessage::Input(
								InputCommand::Search {
									term: String::new(),
									backward: true
								}
							)));
							Some(InputAction::Redraw)
						}
						'f' => self.trigger_link_mode(LinkActionKind::Follow),
						'F' => self.trigger_link_mode(LinkActionKind::Display),
						'c' => self.trigger_link_mode(LinkActionKind::Copy),
						'v' | 'V' => Some(InputAction::ExtractPageText {
							page: self.page,
							open_visual: true
						}),
						'd' => {
							let is_dual = self
								.page_constraints
								.max_wide
								.is_some_and(|w| w.get() >= 2);
							self.page_constraints.max_wide = if is_dual {
								NonZeroUsize::new(1)
							} else {
								NonZeroUsize::new(2)
							};
							if !is_dual {
								self.zoom = None;
							}
							self.last_render.rect = Rect::default();
							self.save_state();
							Some(InputAction::SwitchRenderZoom(FitOrFill::Fit))
						}
						'w' => {
							self.page_constraints.max_wide = NonZeroUsize::new(1);
							self.last_render.rect = Rect::default();
							self.save_state();
							Some(InputAction::Redraw)
						}
						'D' => {
							self.page_constraints.first_page_solo =
								!self.page_constraints.first_page_solo;
							self.last_render.rect = Rect::default();
							self.save_state();
							Some(InputAction::Redraw)
						}
						'R' => Some(InputAction::Reload),
						'a' if self.is_kitty => {
							self.zoom = None;
							self.last_render.rect = Rect::default();
							self.save_state();
							Some(InputAction::SwitchRenderZoom(FitOrFill::Fit))
						}
						's' if self.is_kitty => {
							if self.zoom.is_none() {
								self.zoom = Some(Zoom::default());
							}
							self.last_render.rect = Rect::default();
							self.save_state();
							Some(InputAction::SwitchRenderZoom(FitOrFill::Fill))
						}
						'n' => self.search_step(!self.search_backward),
						'N' => self.search_step(self.search_backward),
						'+' => self.zoom_in(),
						'-' => self.zoom_out(),
						'=' => self.zoom_original(),
						'l' if can_zoom => self.update_zoom(|z| z.pan(Direction::Right)),
						'h' if can_zoom => self.update_zoom(|z| z.pan(Direction::Left)),
						'j' if can_zoom =>
							if self
								.zoom
								.is_some_and(|z| z.cell_pan_from_top >= z.max_pan_top)
							{
								self.change_page(PageChange::Next, ChangeAmount::WholeScreen, 1)
							} else {
								self.update_zoom(|z| z.pan(Direction::Down))
							},
						'k' if can_zoom =>
							if self.zoom.is_some_and(|z| z.cell_pan_from_top == 0) {
								self.change_page(PageChange::Prev, ChangeAmount::WholeScreen, 1)
							} else {
								self.update_zoom(|z| z.pan(Direction::Up))
							},
						'l' => self.change_page(
							PageChange::Next,
							ChangeAmount::Single,
							count.unwrap_or(1)
						),
						'h' => self.change_page(
							PageChange::Prev,
							ChangeAmount::Single,
							count.unwrap_or(1)
						),
						'j' => self.change_page(
							PageChange::Next,
							ChangeAmount::WholeScreen,
							count.unwrap_or(1)
						),
						'k' => self.change_page(
							PageChange::Prev,
							ChangeAmount::WholeScreen,
							count.unwrap_or(1)
						),
						'0' if can_zoom => self.update_zoom(Zoom::pan_left),
						'$' if can_zoom => self.update_zoom(Zoom::pan_right),
						'r' => {
							self.state.rotate_steps = (self.state.rotate_steps + 1) % 4;
							self.save_state();
							Some(InputAction::Rotate)
						}
						_ => None
					},
					KeyCode::Backspace => self.change_page(
						PageChange::Prev,
						ChangeAmount::WholeScreen,
						count.unwrap_or(1)
					),
					KeyCode::Home => self.jump_with_history(MarkPos {
						page: 0,
						pan_left: 0,
						pan_top: 0
					}),
					KeyCode::End => {
						let last = self.rendered.len().saturating_sub(1);
						self.jump_with_history(MarkPos {
							page: last,
							pan_left: 0,
							pan_top: 0
						})
					}
					KeyCode::PageDown => self.change_page(
						PageChange::Next,
						ChangeAmount::Single,
						count.unwrap_or(1)
					),
					KeyCode::PageUp => self.change_page(
						PageChange::Prev,
						ChangeAmount::Single,
						count.unwrap_or(1)
					),
					KeyCode::Right => {
						if alt {
							self.change_page(
								PageChange::Next,
								ChangeAmount::Single,
								count.unwrap_or(1)
							)
						} else if can_zoom {
							self.update_zoom(|z| z.pan(Direction::Right))
						} else {
							self.change_page(
								PageChange::Next,
								ChangeAmount::Single,
								count.unwrap_or(1)
							)
						}
					}
					KeyCode::Left => {
						if alt {
							self.change_page(
								PageChange::Prev,
								ChangeAmount::Single,
								count.unwrap_or(1)
							)
						} else if can_zoom {
							self.update_zoom(|z| z.pan(Direction::Left))
						} else {
							self.change_page(
								PageChange::Prev,
								ChangeAmount::Single,
								count.unwrap_or(1)
							)
						}
					}
					KeyCode::Down => {
						if can_zoom {
							if self
								.zoom
								.is_some_and(|z| z.cell_pan_from_top >= z.max_pan_top)
							{
								self.change_page(PageChange::Next, ChangeAmount::WholeScreen, 1)
							} else {
								self.update_zoom(|z| z.pan(Direction::Down))
							}
						} else {
							self.change_page(
								PageChange::Next,
								ChangeAmount::WholeScreen,
								count.unwrap_or(1)
							)
						}
					}
					KeyCode::Up => {
						if can_zoom {
							if self.zoom.is_some_and(|z| z.cell_pan_from_top == 0) {
								self.change_page(PageChange::Prev, ChangeAmount::WholeScreen, 1)
							} else {
								self.update_zoom(|z| z.pan(Direction::Up))
							}
						} else {
							self.change_page(
								PageChange::Prev,
								ChangeAmount::WholeScreen,
								count.unwrap_or(1)
							)
						}
					}
					_ => None
				}
			}
			Event::Mouse(mouse) => {
				if !matches!(self.overlay, Overlay::None) {
					return None;
				}
				let mut handle_scroll = |mut direction: Direction| {
					if can_zoom {
						if mouse.modifiers.contains(KeyModifiers::CONTROL) {
							match direction {
								Direction::Up => self.update_zoom(Zoom::step_in),
								Direction::Down => self.update_zoom(Zoom::step_out),
								_ => None
							}
						} else {
							if mouse.modifiers.contains(KeyModifiers::SHIFT) {
								direction = direction.flip_mouse_xy();
							}
							self.update_zoom(|z| z.pan(direction))
						}
					} else {
						let (change, amount) = match direction {
							Direction::Right => (PageChange::Next, ChangeAmount::Single),
							Direction::Down => (PageChange::Next, ChangeAmount::WholeScreen),
							Direction::Left => (PageChange::Prev, ChangeAmount::Single),
							Direction::Up => (PageChange::Prev, ChangeAmount::WholeScreen)
						};
						self.change_page(change, amount, 1)
					}
				};
				match mouse.kind {
					MouseEventKind::ScrollRight => handle_scroll(Direction::Right),
					MouseEventKind::ScrollDown => handle_scroll(Direction::Down),
					MouseEventKind::ScrollLeft => handle_scroll(Direction::Left),
					MouseEventKind::ScrollUp => handle_scroll(Direction::Up),
					_ => None
				}
			}
			Event::Resize(_, _) => Some(InputAction::Redraw),
			_ => None
		}
	}

	fn handle_inputbar_key(&mut self, code: KeyCode, ctrl: bool) -> Option<InputAction> {
		match code {
			KeyCode::Tab
				if let BottomMessage::Input(InputCommand::Command(ref mut cmd)) =
					self.bottom_msg =>
			{
				if let Some(completed) = complete_command(cmd, &self.state.bookmarks) {
					*cmd = completed;
				}
				Some(InputAction::Redraw)
			}
			KeyCode::Char('h') if ctrl => {
				if let BottomMessage::Input(
					InputCommand::Search {
						term: ref mut s, ..
					}
					| InputCommand::Command(ref mut s)
				) = self.bottom_msg
				{
					s.pop();
				}
				Some(InputAction::Redraw)
			}
			KeyCode::Char('u') if ctrl => {
				if let BottomMessage::Input(
					InputCommand::Search {
						term: ref mut s, ..
					}
					| InputCommand::Command(ref mut s)
				) = self.bottom_msg
				{
					s.clear();
				}
				Some(InputAction::Redraw)
			}
			KeyCode::Char('w') if ctrl => {
				if let BottomMessage::Input(
					InputCommand::Search {
						term: ref mut s, ..
					}
					| InputCommand::Command(ref mut s)
				) = self.bottom_msg
				{
					let trimmed = s.trim_end();
					let new_len = trimmed
						.rfind(char::is_whitespace)
						.map_or(0, |idx| idx + 1);
					s.truncate(new_len);
				}
				Some(InputAction::Redraw)
			}
			KeyCode::Backspace => {
				if let BottomMessage::Input(
					InputCommand::Search {
						term: ref mut s, ..
					}
					| InputCommand::Command(ref mut s)
				) = self.bottom_msg
				{
					s.pop();
				}
				Some(InputAction::Redraw)
			}
			KeyCode::Char(c)
				if matches!(self.bottom_msg, BottomMessage::Input(InputCommand::MarkSet)) =>
			{
				self.set_msg(MessageSetting::Pop);
				if c.is_ascii_alphanumeric() {
					let pos = self.current_mark_pos();
					self.state.set_mark(c, pos);
					self.save_state();
					self.set_msg(MessageSetting::Some(BottomMessage::Info(format!(
						"Mark '{c}' set at page {}",
						self.page + 1
					))));
				}
				Some(InputAction::Redraw)
			}
			KeyCode::Char(c)
				if matches!(
					self.bottom_msg,
					BottomMessage::Input(InputCommand::MarkJump)
				) =>
			{
				self.set_msg(MessageSetting::Pop);
				if let Some(target) = self.state.get_mark(c) {
					return self.jump_with_history(target);
				}
				self.set_msg(MessageSetting::Some(BottomMessage::Error(format!(
					"Mark '{c}' is not set"
				))));
				Some(InputAction::Redraw)
			}
			KeyCode::Char('j') if ctrl => self.submit_inputbar(),
			KeyCode::Enter => self.submit_inputbar(),
			KeyCode::Char(c) if !ctrl => {
				if let BottomMessage::Input(
					InputCommand::Search {
						term: ref mut s, ..
					}
					| InputCommand::Command(ref mut s)
				) = self.bottom_msg
				{
					s.push(c);
				}
				Some(InputAction::Redraw)
			}
			_ => None
		}
	}

	fn submit_inputbar(&mut self) -> Option<InputAction> {
		let mut default = BottomMessage::default();
		std::mem::swap(&mut self.bottom_msg, &mut default);
		let BottomMessage::Input(cmd) = default else {
			std::mem::swap(&mut self.bottom_msg, &mut default);
			return None;
		};

		match cmd {
			InputCommand::Search { term, backward } => {
				self.search_backward = backward;
				if !term.is_empty() {
					self.last_search_term.clone_from(&term);
					self.set_msg(MessageSetting::Some(BottomMessage::SearchResults(
						term.clone()
					)));
				} else {
					self.set_msg(MessageSetting::Reset);
				}

				for img in &mut self.rendered {
					img.num_results = None;
				}
				Some(InputAction::Search(term))
			}
			InputCommand::Command(cmd_str) => self.execute_colon_command(&cmd_str),
			InputCommand::MarkSet | InputCommand::MarkJump => Some(InputAction::Redraw)
		}
	}

	#[expect(clippy::unnecessary_wraps)]
	fn update_zoom(&mut self, f: impl FnOnce(&mut Zoom)) -> Option<InputAction> {
		if let Some(z) = &mut self.zoom {
			f(z);
		}
		self.last_render.rect = Rect::default();
		self.save_state();
		Some(InputAction::Redraw)
	}

	pub fn show_error(&mut self, err: RenderError) {
		self.set_msg(MessageSetting::Some(BottomMessage::Error(match err {
			RenderError::Notify(e) => format!("Auto-reload failed: {e}"),
			RenderError::Doc(e) => format!("Couldn't process document: {e}"),
			RenderError::Converting(e) => format!("Couldn't convert page after rendering: {e}")
		})));
	}

	fn set_page(&mut self, page: usize) {
		if page != self.page {
			self.last_render.rect = Rect::default();
			self.page = page;
			if let Some(zoom) = &mut self.zoom {
				zoom.cell_pan_from_top = 0;
				zoom.cell_pan_from_left = 0;
				zoom.max_pan_top = u16::MAX;
			}
		}
	}

	pub fn set_msg(&mut self, msg: MessageSetting) {
		match msg {
			MessageSetting::Some(mut msg) => {
				std::mem::swap(&mut self.bottom_msg, &mut msg);
				self.prev_msg = Some(msg);
			}
			MessageSetting::Default => self.set_msg(MessageSetting::Some(BottomMessage::default())),
			MessageSetting::Reset => {
				self.prev_msg = None;
				self.bottom_msg = BottomMessage::default();
			}
			MessageSetting::Pop =>
				if !matches!(self.overlay, Overlay::None) {
					self.last_render.rect = Rect::default();
					self.overlay = Overlay::None;
				} else {
					self.bottom_msg = self.prev_msg.take().unwrap_or_default();
				},
		}
	}

	fn centered_modal(frame: &mut Frame<'_>, title: &str, w_pct: u16, h_pct: u16) -> Rect {
		let area = frame.area();
		frame.render_widget(Clear, area);
		let v_layout = Layout::vertical([
			Constraint::Percentage((100 - h_pct) / 2),
			Constraint::Percentage(h_pct),
			Constraint::Percentage((100 - h_pct) / 2)
		])
		.split(area);
		let h_layout = Layout::horizontal([
			Constraint::Percentage((100 - w_pct) / 2),
			Constraint::Percentage(w_pct),
			Constraint::Percentage((100 - w_pct) / 2)
		])
		.split(v_layout[1]);

		let block = Block::new()
			.title(format!(" {title} "))
			.padding(Padding::proportional(1))
			.borders(Borders::ALL)
			.border_set(border::ROUNDED)
			.border_style(Color::Blue);
		let inner = block.inner(h_layout[1]);
		frame.render_widget(block, h_layout[1]);
		inner
	}

	fn render_toc_overlay(&self, frame: &mut Frame<'_>) {
		let inner = Self::centered_modal(
			frame,
			"Index / TOC (Tab/Esc: close | j/k: move | h/l/zc/zo: fold | Enter/Space: jump)",
			80,
			80
		);
		let visible_h = usize::from(inner.height.max(1));
		let start = self.toc.cursor.saturating_sub(visible_h / 2);
		let lines = self
			.toc
			.flat
			.iter()
			.enumerate()
			.skip(start)
			.take(visible_h)
			.map(|(idx, item)| {
				let indent = "  ".repeat(item.depth);
				let fold_icon = if item.has_children {
					if item.expanded { "▾ " } else { "▸ " }
				} else {
					"  "
				};
				let page_str = item
					.page
					.map_or_else(String::new, |p| format!(" (p.{})", p + 1));
				let text = format!("{indent}{fold_icon}{}{page_str}", item.title);
				if idx == self.toc.cursor {
					Line::from(Span::styled(
						text,
						Style::new()
							.fg(Color::Black)
							.bg(Color::Cyan)
							.add_modifier(Modifier::BOLD)
					))
				} else {
					Line::from(Span::raw(text))
				}
			})
			.collect::<Vec<_>>();

		frame.render_widget(Paragraph::new(lines), inner);
	}

	fn render_info_overlay(&self, frame: &mut Frame<'_>) {
		let inner = Self::centered_modal(frame, "Document Information (Esc/q: close)", 75, 70);
		let mut lines = self
			.doc_info
			.iter()
			.map(|(k, v)| {
				Line::from(vec![
					Span::styled(
						format!("{k:<14}: "),
						Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
					),
					Span::raw(v.as_str()),
				])
			})
			.collect::<Vec<_>>();

		lines.push(Line::from(""));
		lines.push(Line::from(Span::styled(
			format!(
				"Marks saved   : {}   |   Bookmarks saved: {}",
				self.state.marks.len(),
				self.state.bookmarks.len()
			),
			Style::new().fg(Color::Yellow)
		)));

		frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
	}

	fn render_visual_overlay(frame: &mut Frame<'_>, vs: &VisualTextState) {
		let title = format!(
			"Visual Mode - Page {} (j/k: move | v/V: select | o: swap | y/Y: yank | Esc: exit)",
			vs.page + 1
		);
		let inner = Self::centered_modal(frame, &title, 85, 85);
		let visible_h = usize::from(inner.height.max(1));
		let start = vs.cursor.saturating_sub(visible_h / 2);
		let (sel_start, sel_end) = vs.selected_range();

		let lines = vs
			.lines
			.iter()
			.enumerate()
			.skip(start)
			.take(visible_h)
			.map(|(idx, line)| {
				let text = format!("{:>4} │ {line}", idx + 1);
				let is_cursor = idx == vs.cursor;
				let is_selected = vs.anchor.is_some() && idx >= sel_start && idx <= sel_end;
				let style = if is_cursor {
					Style::new()
						.fg(Color::Black)
						.bg(Color::Cyan)
						.add_modifier(Modifier::BOLD)
				} else if is_selected {
					Style::new().fg(Color::Black).bg(Color::Yellow)
				} else {
					Style::default()
				};
				Line::from(Span::styled(text, style))
			})
			.collect::<Vec<_>>();

		frame.render_widget(Paragraph::new(lines), inner);
	}

	fn render_picker_overlay(frame: &mut Frame<'_>, picker: &ListPickerState) {
		let inner = Self::centered_modal(frame, &picker.title, 75, 65);
		let visible_h = usize::from(inner.height.max(1));
		let start = picker.cursor.saturating_sub(visible_h / 2);

		let lines = picker
			.items
			.iter()
			.enumerate()
			.skip(start)
			.take(visible_h)
			.map(|(idx, item)| {
				if idx == picker.cursor {
					Line::from(Span::styled(
						item.label.clone(),
						Style::new()
							.fg(Color::Black)
							.bg(Color::Cyan)
							.add_modifier(Modifier::BOLD)
					))
				} else {
					Line::from(Span::raw(item.label.clone()))
				}
			})
			.collect::<Vec<_>>();

		frame.render_widget(Paragraph::new(lines), inner);
	}

	pub fn render_help_msg(&self, frame: &mut Frame<'_>) {
		let inner = Self::centered_modal(frame, "Zathura Keybindings Help (Esc/F1: close)", 85, 88);
		let help_sections = [
			Text::from(HELP_PAGE),
			Text::from(""),
			if self.is_kitty {
				Text::from(KITTY_HELP)
			} else {
				Text::from("Not using kitty, kitty-specific keybindings hidden")
					.style(Color::DarkGray)
			}
		];

		let mut area = inner;
		for section in help_sections {
			let section_lines = section.lines.len();
			let span = Paragraph::new(section).wrap(Wrap { trim: false });
			frame.render_widget(span, area);
			let step = u16::try_from(section_lines).unwrap_or(0);
			area.y = area.y.saturating_add(step);
			area.height = area.height.saturating_sub(step);
		}
	}
}

static HELP_PAGE: &str = "\
Navigation & Marks (Zathura):
  J / K, PgDn / PgUp   : Next / Previous page (supports [count])
  h / j / k / l        : Scroll left / down / up / right
  Ctrl+d / Ctrl+u      : Scroll half page down / up
  Ctrl+f / Ctrl+b, Spc : Scroll full screen down / up
  H / L, P             : Scroll to page top / bottom, Snap to page (P)
  gg / G, [N]gg / [N]G : First / Last page, or jump to page N
  m<char> / '<char>    : Set quickmark / Jump to quickmark
  Ctrl+o / Ctrl+i      : Jumplist backward / forward
  Ctrl+j / Ctrl+k      : Bisect forward / backward

Index (TOC), Links & Visual Mode:
  Tab                  : Toggle Index (j/k, h/l/zc/zo/zM/zR, Enter/Space)
  f / F / c            : Follow link / Display link / Copy link
  v / V, gv            : Visual mode (j/k, v/V select, o swap, y/Y yank)
  Y                    : Copy opened file path to clipboard

Display, Search & Command Bar:
  d / D / w            : Toggle dual-page / Cycle first-page col / Single-page
  r / R / Ctrl+r       : Rotate 90° CW / Reload / Toggle invert colors
  F5 / F11 / Ctrl+n    : Toggle fullscreen / statusbar
  / / ?, n / N         : Search forward / backward, Next / Prev match
  Esc / Ctrl+c         : Abort input / Clear search highlight
  q                    : Quit tdf
  :                    : Command bar (:bmark, :bjump, :blist, :bdel, :jumplist,
                         :offset, :mark, :delmarks, :nohl, :info, :help, :q)\
";

static KITTY_HELP: &str = "\
Zoom & Adjust (Zathura):
  a / s                : Adjust best-fit (a) / Adjust width (s)
  zi, zI, +            : Zoom in
  zo, zO, -            : Zoom out
  z0, zz, zZ, =        : Reset zoom to original\
";

pub enum InputAction {
	Redraw,
	JumpingToPage(usize),
	Search(String),
	QuitApp,
	Invert,
	Rotate,
	Reload,
	Fullscreen,
	SwitchRenderZoom(crate::FitOrFill),
	ExtractPageText { page: usize, open_visual: bool }
}

#[derive(Copy, Clone)]
enum PageChange {
	Prev,
	Next
}

#[derive(Copy, Clone)]
enum ChangeAmount {
	WholeScreen,
	Single
}

pub enum MessageSetting {
	Some(BottomMessage),
	Default,
	Reset,
	Pop
}

#[cfg(test)]
mod tests {
	use std::num::NonZeroU32;

	use super::*;

	#[test]
	fn render_zoomed_handles_pane_growth_and_zero_area_without_panic() {
		let mut img = MaybeTransferred::Transferred(NonZeroU32::new(1).unwrap());
		let mut zoom = Zoom {
			level: -6,
			cell_pan_from_left: 0,
			cell_pan_from_top: 0,
			max_pan_top: u16::MAX
		};
		let font = FontSize {
			width: 14,
			height: 28
		};

		let res = Tui::render_zoomed(
			Rect::new(2, 4, 150, 29),
			font,
			&mut zoom,
			&mut img,
			0,
			(72, 28),
			(72 * 14, 28 * 28)
		);
		assert!(matches!(res, KittyDisplay::DisplayImages(_)));

		let zero_res = Tui::render_zoomed(
			Rect::new(0, 0, 0, 0),
			font,
			&mut zoom,
			&mut img,
			0,
			(72, 28),
			(72 * 14, 28 * 28)
		);
		assert!(matches!(zero_res, KittyDisplay::ClearImages));
	}

	#[test]
	fn render_zoomed_uses_exact_pixel_width_without_cropping() {
		let mut img = MaybeTransferred::Transferred(NonZeroU32::new(1).unwrap());
		let mut zoom = Zoom {
			level: -7,
			cell_pan_from_left: 0,
			cell_pan_from_top: 0,
			max_pan_top: u16::MAX
		};
		let font = FontSize {
			width: 12,
			height: 25
		};

		// Exact 91x37 pane -> page area 87x29; rendered at matching 12x25 cell size -> 1044x1429 px.
		let res = Tui::render_zoomed(
			Rect::new(2, 4, 87, 29),
			font,
			&mut zoom,
			&mut img,
			659,
			(87, 57),
			(1044, 1429)
		);
		let KittyDisplay::DisplayImages(imgs) = res else {
			panic!("expected DisplayImages");
		};
		assert_eq!(imgs[0].display_loc.width, 1044);
		assert_eq!(imgs[0].display_loc.columns, 61);
		assert_eq!(imgs[0].display_loc.rows, 29);

		// Non-multiple-of-cell-size landscape image (e.g. 1800x1013) must preserve full width & exact aspect.
		let mut zoom0 = Zoom::default();
		let res_landscape = Tui::render_zoomed(
			Rect::new(2, 4, 150, 29),
			font,
			&mut zoom0,
			&mut img,
			0,
			(150, 40),
			(1800, 1013)
		);
		let KittyDisplay::DisplayImages(imgs_l) = res_landscape else {
			panic!("expected DisplayImages");
		};
		assert_eq!(imgs_l[0].display_loc.width, 1800);
		assert_eq!(imgs_l[0].display_loc.height, 725);

		let mut zoom_out = Zoom {
			level: -7,
			..Zoom::default()
		};
		let res_landscape_out = Tui::render_zoomed(
			Rect::new(2, 4, 150, 29),
			font,
			&mut zoom_out,
			&mut img,
			0,
			(150, 40),
			(1800, 1013)
		);
		let KittyDisplay::DisplayImages(imgs_lo) = res_landscape_out else {
			panic!("expected DisplayImages");
		};
		assert_eq!(imgs_lo[0].display_loc.columns, 107);
		assert_eq!(imgs_lo[0].display_loc.rows, 29);
		assert_eq!(imgs_lo[0].display_loc.width, 1794);
		assert_eq!(imgs_lo[0].display_loc.height, 1013);
	}

	#[test]
	fn render_skips_stale_vertically_oversized_fit_page() {
		use ratatui::{Terminal, backend::TestBackend};

		let mut term = Terminal::new(TestBackend::new(91, 18)).unwrap();
		let mut tui = Tui::new(
			"test.pdf".to_string(),
			PathBuf::from("/tmp/nonexistent-tdf-test.pdf"),
			None,
			false,
			true
		);
		tui.zoom = None;
		tui.set_n_pages(1);
		tui.rendered[0].img = Some(ConvertedImage::Kitty {
			img: MaybeTransferred::Transferred(NonZeroU32::new(1).unwrap()),
			cell_w: 44,
			cell_h: 29,
			px_w: 528,
			px_h: 725
		});

		let mut display = KittyDisplay::NoChange;
		term.draw(|f| {
			let layout = Tui::main_layout(f, false);
			display = tui.render(f, &layout, FontSize {
				width: 12,
				height: 25
			});
		})
		.unwrap();

		assert!(matches!(display, KittyDisplay::ClearImages));
	}
}
