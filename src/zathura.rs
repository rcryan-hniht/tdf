use std::{
	env, fs,
	io::{Read as _, Write as _},
	path::{Path, PathBuf},
	process::{Command, Stdio}
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MarkPos {
	pub page: usize,
	pub pan_left: u16,
	pub pan_top: u16
}

#[derive(Debug, Clone, Default)]
pub struct FileState {
	pub page: usize,
	pub page_offset: i32,
	pub rotate_steps: u8,
	pub invert: bool,
	pub dual_page: bool,
	pub first_page_solo: bool,
	pub zoom_level: Option<i16>,
	pub pan_left: u16,
	pub pan_top: u16,
	pub marks: Vec<(char, MarkPos)>,
	pub bookmarks: Vec<(String, usize)>
}

impl FileState {
	pub fn set_mark(&mut self, key: char, pos: MarkPos) {
		if let Some((_, existing)) = self.marks.iter_mut().find(|(k, _)| *k == key) {
			*existing = pos;
		} else {
			self.marks.push((key, pos));
		}
	}

	#[must_use]
	pub fn get_mark(&self, key: char) -> Option<MarkPos> {
		self.marks
			.iter()
			.find_map(|(k, pos)| (*k == key).then_some(*pos))
	}

	pub fn delete_marks(&mut self, spec: &str) -> usize {
		let before = self.marks.len();
		if spec.trim() == "!" {
			self.marks.clear();
		} else {
			self.marks.retain(|(k, _)| !spec.contains(*k));
		}
		before.saturating_sub(self.marks.len())
	}

	pub fn set_bookmark(&mut self, name: String, page: usize) {
		if let Some((_, existing_page)) = self.bookmarks.iter_mut().find(|(n, _)| *n == name) {
			*existing_page = page;
		} else {
			self.bookmarks.push((name, page));
		}
	}

	#[must_use]
	pub fn get_bookmark(&self, name: &str) -> Option<usize> {
		self.bookmarks
			.iter()
			.find_map(|(n, page)| (n == name).then_some(*page))
	}

	pub fn delete_bookmark(&mut self, name: &str) -> bool {
		let before = self.bookmarks.len();
		self.bookmarks.retain(|(n, _)| n != name);
		self.bookmarks.len() < before
	}
}

fn state_file_path() -> Option<PathBuf> {
	let base = env::var_os("XDG_DATA_HOME")
		.map(PathBuf::from)
		.or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
	Some(base.join("tdf").join("state.tsv"))
}

fn escape_field(input: &str) -> String {
	let mut out = String::with_capacity(input.len());
	for ch in input.chars() {
		match ch {
			'\\' => out.push_str("\\\\"),
			'\t' => out.push_str("\\t"),
			'\n' => out.push_str("\\n"),
			'|' => out.push_str("\\p"),
			',' => out.push_str("\\c"),
			':' => out.push_str("\\d"),
			other => out.push(other)
		}
	}
	out
}

fn unescape_field(input: &str) -> String {
	let mut out = String::with_capacity(input.len());
	let mut chars = input.chars();
	while let Some(ch) = chars.next() {
		if ch == '\\' {
			match chars.next() {
				Some('\\') => out.push('\\'),
				Some('t') => out.push('\t'),
				Some('n') => out.push('\n'),
				Some('p') => out.push('|'),
				Some('c') => out.push(','),
				Some('d') => out.push(':'),
				Some(other) => {
					out.push('\\');
					out.push(other);
				}
				None => out.push('\\')
			}
		} else {
			out.push(ch);
		}
	}
	out
}

fn serialize_state(state: &FileState) -> String {
	let zoom_str = state
		.zoom_level
		.map_or_else(|| "none".to_string(), |z| z.to_string());
	let marks_str = state
		.marks
		.iter()
		.map(|(k, pos)| format!("{k}:{}:{}:{}", pos.page, pos.pan_left, pos.pan_top))
		.collect::<Vec<_>>()
		.join(",");
	let bmarks_str = state
		.bookmarks
		.iter()
		.map(|(name, page)| format!("{}:{page}", escape_field(name)))
		.collect::<Vec<_>>()
		.join(",");

	format!(
		"{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
		state.page,
		state.page_offset,
		state.rotate_steps,
		u8::from(state.invert),
		u8::from(state.dual_page),
		u8::from(state.first_page_solo),
		zoom_str,
		state.pan_left,
		state.pan_top,
		marks_str,
		bmarks_str
	)
}

fn parse_state(cols: &[&str]) -> Option<FileState> {
	if cols.len() < 11 {
		return None;
	}
	let page = cols[0].parse().ok()?;
	let page_offset = cols[1].parse().ok()?;
	let rotate_steps = cols[2].parse().ok()?;
	let invert = cols[3] == "1";
	let dual_page = cols[4] == "1";
	let first_page_solo = cols[5] == "1";
	let zoom_level = if cols[6] == "none" {
		None
	} else {
		cols[6].parse().ok()
	};
	let pan_left = cols[7].parse().ok()?;
	let pan_top = cols[8].parse().ok()?;

	let mut marks = Vec::new();
	if !cols[9].is_empty() {
		for item in cols[9].split(',') {
			let mut parts = item.split(':');
			let Some(k_str) = parts.next() else {
				continue;
			};
			let Some(key) = k_str.chars().next() else {
				continue;
			};
			let Some(m_page) = parts.next().and_then(|s| s.parse().ok()) else {
				continue;
			};
			let m_left = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
			let m_top = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
			marks.push((key, MarkPos {
				page: m_page,
				pan_left: m_left,
				pan_top: m_top
			}));
		}
	}

	let mut bookmarks = Vec::new();
	if !cols[10].is_empty() {
		for item in cols[10].split(',') {
			let Some((raw_name, raw_page)) = item.rsplit_once(':') else {
				continue;
			};
			if let Ok(b_page) = raw_page.parse() {
				bookmarks.push((unescape_field(raw_name), b_page));
			}
		}
	}

	Some(FileState {
		page,
		page_offset,
		rotate_steps,
		invert,
		dual_page,
		first_page_solo,
		zoom_level,
		pan_left,
		pan_top,
		marks,
		bookmarks
	})
}

#[must_use]
pub fn load_file_state(file_path: &Path) -> Option<FileState> {
	let db_path = state_file_path()?;
	let mut file = fs::File::open(db_path).ok()?;
	let mut content = String::new();
	file.read_to_string(&mut content).ok()?;
	let key = escape_field(&file_path.to_string_lossy());

	for line in content.lines() {
		let cols = line.split('\t').collect::<Vec<_>>();
		if cols.first().copied() == Some(key.as_str()) {
			return parse_state(&cols[1..]);
		}
	}
	None
}

pub fn save_file_state(file_path: &Path, state: &FileState) {
	let Some(db_path) = state_file_path() else {
		return;
	};
	if let Some(parent) = db_path.parent() {
		let _ = fs::create_dir_all(parent);
	}
	let key = escape_field(&file_path.to_string_lossy());
	let serialized = format!("{key}\t{}", serialize_state(state));

	let mut lines = Vec::new();
	let mut replaced = false;
	if let Ok(mut existing_file) = fs::File::open(&db_path) {
		let mut content = String::new();
		if existing_file.read_to_string(&mut content).is_ok() {
			for line in content.lines() {
				if line.split('\t').next() == Some(key.as_str()) {
					lines.push(serialized.clone());
					replaced = true;
				} else if !line.trim().is_empty() {
					lines.push(line.to_string());
				}
			}
		}
	}
	if !replaced {
		lines.push(serialized);
	}
	if lines.len() > 500 {
		let drain_count = lines.len() - 500;
		lines.drain(..drain_count);
	}
	let out = lines.join("\n") + "\n";
	let _ = fs::write(db_path, out);
}

#[derive(Debug, Clone)]
pub struct JumpList {
	entries: Vec<MarkPos>,
	cur: usize,
	max_size: usize
}

impl Default for JumpList {
	fn default() -> Self {
		Self {
			entries: Vec::new(),
			cur: 0,
			max_size: 200
		}
	}
}

impl JumpList {
	pub fn record(&mut self, pos: MarkPos) {
		if self.cur < self.entries.len() {
			self.entries.truncate(self.cur);
		}
		if self.entries.last().copied() != Some(pos) {
			self.entries.push(pos);
			if self.entries.len() > self.max_size {
				self.entries.remove(0);
			}
		}
		self.cur = self.entries.len();
	}

	pub fn go_back(&mut self, current: MarkPos) -> Option<MarkPos> {
		if self.entries.is_empty() {
			return None;
		}
		if self.cur == self.entries.len() && self.entries.last().copied() != Some(current) {
			self.entries.push(current);
			self.cur = self.entries.len() - 1;
		}
		while self.cur > 0 {
			self.cur -= 1;
			let target = self.entries[self.cur];
			if target.page != current.page
				|| target.pan_top != current.pan_top
				|| target.pan_left != current.pan_left
			{
				return Some(target);
			}
		}
		None
	}

	pub fn go_forward(&mut self, current: MarkPos) -> Option<MarkPos> {
		while self.cur + 1 < self.entries.len() {
			self.cur += 1;
			let target = self.entries[self.cur];
			if target.page != current.page
				|| target.pan_top != current.pan_top
				|| target.pan_left != current.pan_left
			{
				return Some(target);
			}
		}
		None
	}

	#[must_use]
	pub fn entries(&self) -> &[MarkPos] {
		&self.entries
	}
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BisectState {
	range: Option<(usize, usize)>
}

impl BisectState {
	pub fn reset(&mut self) {
		self.range = None;
	}

	pub fn step(
		&mut self,
		current: usize,
		total_pages: usize,
		forward: bool
	) -> Option<(usize, usize, usize)> {
		if total_pages <= 1 {
			return None;
		}
		let (mut low, mut high) = self.range.unwrap_or((0, total_pages - 1));
		if forward {
			low = current.min(high);
		} else {
			high = current.max(low);
		}
		let target = usize::midpoint(low, high);
		self.range = Some((low, high));
		Some((target, low, high))
	}
}

#[derive(Debug, Clone)]
pub struct TocNode {
	pub title: String,
	pub page: Option<usize>,
	pub uri: Option<String>,
	pub children: Vec<Self>,
	pub expanded: bool
}

#[derive(Debug, Clone)]
pub struct FlatTocItem {
	pub depth: usize,
	pub title: String,
	pub page: Option<usize>,
	pub uri: Option<String>,
	pub has_children: bool,
	pub expanded: bool,
	pub path: Vec<usize>
}

#[derive(Debug, Clone, Default)]
pub struct TocState {
	pub roots: Vec<TocNode>,
	pub flat: Vec<FlatTocItem>,
	pub cursor: usize,
	pub pending_z: bool,
	pub pending_g: bool
}

impl TocState {
	#[must_use]
	pub fn new(roots: Vec<TocNode>) -> Self {
		let mut state = Self {
			roots,
			flat: Vec::new(),
			cursor: 0,
			pending_z: false,
			pending_g: false
		};
		state.rebuild_flat();
		state
	}

	pub fn rebuild_flat(&mut self) {
		self.flat.clear();
		let mut path = Vec::new();
		Self::flatten_nodes(&self.roots, 0, &mut path, &mut self.flat);
		if !self.flat.is_empty() {
			self.cursor = self.cursor.min(self.flat.len() - 1);
		} else {
			self.cursor = 0;
		}
	}

	fn flatten_nodes(
		nodes: &[TocNode],
		depth: usize,
		path: &mut Vec<usize>,
		out: &mut Vec<FlatTocItem>
	) {
		for (idx, node) in nodes.iter().enumerate() {
			path.push(idx);
			out.push(FlatTocItem {
				depth,
				title: node.title.clone(),
				page: node.page,
				uri: node.uri.clone(),
				has_children: !node.children.is_empty(),
				expanded: node.expanded,
				path: path.clone()
			});
			if node.expanded && !node.children.is_empty() {
				Self::flatten_nodes(&node.children, depth + 1, path, out);
			}
			path.pop();
		}
	}

	fn node_mut_at_path<'a>(
		 mut nodes: &'a mut [TocNode],
		path: &[usize]
	) -> Option<&'a mut TocNode> {
		let (&last, prefix) = path.split_last()?;
		for &idx in prefix {
			nodes = &mut nodes.get_mut(idx)?.children;
		}
		nodes.get_mut(last)
	}

	fn set_expanded_recursive(node: &mut TocNode, expanded: bool) {
		node.expanded = expanded;
		for child in &mut node.children {
			Self::set_expanded_recursive(child, expanded);
		}
	}

	pub fn set_current_expanded(&mut self, expanded: bool, recursive: bool) {
		let Some(item) = self.flat.get(self.cursor) else {
			return;
		};
		let path = item.path.clone();
		if let Some(node) = Self::node_mut_at_path(&mut self.roots, &path) {
			if recursive {
				Self::set_expanded_recursive(node, expanded);
			} else {
				node.expanded = expanded;
			}
			self.rebuild_flat();
		}
	}

	pub fn set_all_expanded(&mut self, expanded: bool) {
		for root in &mut self.roots {
			Self::set_expanded_recursive(root, expanded);
		}
		self.rebuild_flat();
	}

	pub fn sync_to_page(&mut self, current_page: usize) {
		let mut best_idx = None;
		for (idx, item) in self.flat.iter().enumerate() {
			if let Some(p) = item.page
				&& p <= current_page
			{
				best_idx = Some(idx);
			}
		}
		if let Some(idx) = best_idx {
			self.cursor = idx;
		}
	}
}

#[derive(Debug, Clone)]
pub struct PageLink {
	pub page: usize,
	pub dest_page: Option<usize>,
	pub uri: String
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkActionKind {
	Follow,
	Display,
	Copy
}

#[derive(Debug, Clone)]
pub struct VisualTextState {
	pub page: usize,
	pub lines: Vec<String>,
	pub cursor: usize,
	pub anchor: Option<usize>,
	pub pending_g: bool
}

impl VisualTextState {
	#[must_use]
	pub fn new(page: usize, text: &str) -> Self {
		let mut lines = text.lines().map(ToString::to_string).collect::<Vec<_>>();
		if lines.is_empty() {
			lines.push("(No extractable text on this page)".to_string());
		}
		Self {
			page,
			lines,
			cursor: 0,
			anchor: None,
			pending_g: false
		}
	}

	#[must_use]
	pub fn selected_range(&self) -> (usize, usize) {
		match self.anchor {
			Some(a) => (a.min(self.cursor), a.max(self.cursor)),
			None => (self.cursor, self.cursor)
		}
	}

	#[must_use]
	pub fn selected_text(&self) -> String {
		let (start, end) = self.selected_range();
		self.lines
			.get(start..=end)
			.map_or_else(String::new, |slice| slice.join("\n"))
	}
}

#[derive(Debug, Clone)]
pub enum PickerKind {
	Bookmarks,
	JumpList,
	Links(LinkActionKind)
}

#[derive(Debug, Clone)]
pub struct PickerItem {
	pub label: String,
	pub target_page: Option<usize>,
	pub uri: Option<String>,
	pub mark_pos: Option<MarkPos>
}

#[derive(Debug, Clone)]
pub struct ListPickerState {
	pub title: String,
	pub kind: PickerKind,
	pub items: Vec<PickerItem>,
	pub cursor: usize
}

pub const COMMANDS: &[&str] = &[
	"bmark ",
	"bjump ",
	"blist",
	"bdelete ",
	"jumplist",
	"mark ",
	"delmarks ",
	"offset ",
	"nohlsearch",
	"hlsearch",
	"info",
	"yank",
	"reload",
	"help",
	"quit"
];

#[must_use]
pub fn complete_command(input: &str, bookmarks: &[(String, usize)]) -> Option<String> {
	if let Some(arg) = input
		.strip_prefix("bjump ")
		.or_else(|| input.strip_prefix("bdelete "))
		.or_else(|| input.strip_prefix("bdel "))
	{
		let prefix = &input[..input.len() - arg.len()];
		for (name, _) in bookmarks {
			if name.starts_with(arg) && name != arg {
				return Some(format!("{prefix}{name}"));
			}
		}
		return None;
	}

	for &cmd in COMMANDS {
		if cmd.starts_with(input) && cmd != input {
			return Some(cmd.to_string());
		}
	}
	None
}

fn base64_encode(bytes: &[u8]) -> String {
	const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
	for chunk in bytes.chunks(3) {
		let b0 = chunk[0];
		let b1 = chunk.get(1).copied().unwrap_or(0);
		let b2 = chunk.get(2).copied().unwrap_or(0);

		out.push(char::from(TABLE[usize::from(b0 >> 2)]));
		out.push(char::from(
			TABLE[usize::from(((b0 & 0x03) << 4) | (b1 >> 4))]
		));
		if chunk.len() > 1 {
			out.push(char::from(
				TABLE[usize::from(((b1 & 0x0f) << 2) | (b2 >> 6))]
			));
		} else {
			out.push('=');
		}
		if chunk.len() > 2 {
			out.push(char::from(TABLE[usize::from(b2 & 0x3f)]));
		} else {
			out.push('=');
		}
	}
	out
}

pub fn copy_to_clipboard(text: &str, is_tmux: bool) -> Result<(), String> {
	let encoded = base64_encode(text.as_bytes());
	{
		let mut out = std::io::stdout().lock();
		let osc = if is_tmux {
			format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\")
		} else {
			format!("\x1b]52;c;{encoded}\x07")
		};
		let _ = out.write_all(osc.as_bytes());
		let _ = out.flush();
	}

	for (bin, args) in [
		("wl-copy", &[][..]),
		("xclip", &["-selection", "clipboard"][..]),
		("xsel", &["-ib"][..])
	] {
		if let Ok(mut child) = Command::new(bin)
			.args(args)
			.stdin(Stdio::piped())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
		{
			if let Some(mut stdin) = child.stdin.take() {
				let _ = stdin.write_all(text.as_bytes());
			}
			let _ = child.wait();
			return Ok(());
		}
	}

	Ok(())
}

pub fn open_external_uri(uri: &str) -> Result<(), String> {
	Command::new("xdg-open")
		.arg(uri)
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.map(|_| ())
		.map_err(|e| format!("Failed to launch xdg-open: {e}"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn state_roundtrip_and_marks_bookmarks() {
		let mut st = FileState {
			page: 7,
			page_offset: -2,
			rotate_steps: 1,
			invert: true,
			dual_page: true,
			first_page_solo: true,
			zoom_level: Some(3),
			pan_left: 12,
			pan_top: 24,
			marks: Vec::new(),
			bookmarks: Vec::new()
		};
		st.set_mark('a', MarkPos {
			page: 3,
			pan_left: 5,
			pan_top: 10
		});
		st.set_bookmark("chap:1,intro".to_string(), 4);

		let ser = serialize_state(&st);
		let cols = ser.split('\t').collect::<Vec<_>>();
		let parsed = parse_state(&cols).expect("should parse state");

		assert_eq!(parsed.page, 7);
		assert_eq!(parsed.page_offset, -2);
		assert_eq!(parsed.rotate_steps, 1);
		assert!(parsed.invert);
		assert!(parsed.dual_page);
		assert!(parsed.first_page_solo);
		assert_eq!(parsed.zoom_level, Some(3));
		assert_eq!(
			parsed.get_mark('a'),
			Some(MarkPos {
				page: 3,
				pan_left: 5,
				pan_top: 10
			})
		);
		assert_eq!(parsed.get_bookmark("chap:1,intro"), Some(4));
	}

	#[test]
	fn jumplist_and_bisect() {
		let mut jl = JumpList::default();
		let p0 = MarkPos {
			page: 0,
			pan_left: 0,
			pan_top: 0
		};
		let p5 = MarkPos {
			page: 5,
			pan_left: 0,
			pan_top: 0
		};
		let p12 = MarkPos {
			page: 12,
			pan_left: 0,
			pan_top: 0
		};
		jl.record(p0);
		jl.record(p5);
		assert_eq!(jl.go_back(p12), Some(p5));
		assert_eq!(jl.go_back(p5), Some(p0));
		assert_eq!(jl.go_forward(p0), Some(p5));
		assert_eq!(jl.go_forward(p5), Some(p12));

		let mut bs = BisectState::default();
		let (mid1, low1, high1) = bs.step(0, 100, true).unwrap();
		assert_eq!((mid1, low1, high1), (49, 0, 99));
		let (mid2, low2, high2) = bs.step(mid1, 100, false).unwrap();
		assert_eq!((mid2, low2, high2), (24, 0, 49));
	}

	#[test]
	fn toc_fold_and_command_completion() {
		let roots = vec![TocNode {
			title: "Part 1".to_string(),
			page: Some(0),
			uri: None,
			expanded: true,
			children: vec![TocNode {
				title: "Sec 1.1".to_string(),
				page: Some(2),
				uri: None,
				expanded: true,
				children: Vec::new()
			}]
		}];
		let mut toc = TocState::new(roots);
		assert_eq!(toc.flat.len(), 2);
		toc.set_current_expanded(false, false);
		assert_eq!(toc.flat.len(), 1);
		toc.set_all_expanded(true);
		assert_eq!(toc.flat.len(), 2);

		let bmarks = vec![("intro".to_string(), 1)];
		assert_eq!(
			complete_command("bm", &bmarks),
			Some("bmark ".to_string())
		);
		assert_eq!(
			complete_command("bjump in", &bmarks),
			Some("bjump intro".to_string())
		);
	}
}
