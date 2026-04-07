#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChange,
    Copied,
    Unknown(String),
}

#[derive(Debug, Clone)]
pub struct FileSummary {
    pub path: String,
    pub old_path: Option<String>,
    pub added: Option<u64>,
    pub deleted: Option<u64>,
    pub change: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Context,
    Add,
    Delete,
}

#[derive(Debug, Clone)]
pub struct PatchLine {
    pub kind: DiffKind,
    pub old_lineno: Option<usize>,
    pub new_lineno: Option<usize>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct PatchHunk {
    pub header: String,
    pub new_start: usize,
    pub lines: Vec<PatchLine>,
}

#[derive(Debug, Clone)]
pub struct FilePatch {
    pub summary: FileSummary,
    pub hunks: Vec<PatchHunk>,
    pub metadata: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReviewEdge {
    pub base: String,
    pub head: String,
}

impl ReviewEdge {
    pub fn label(&self) -> String {
        format!("{}...{}", self.base, self.head)
    }
}

#[derive(Debug, Clone)]
pub struct LineReference {
    pub old_lineno: Option<usize>,
    pub new_lineno: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Annotation {
    pub id: u64,
    pub file_path: String,
    pub edge: Option<ReviewEdge>,
    pub hunk_header: Option<String>,
    pub scope: AnnotationScope,
    pub body: String,
}

#[derive(Debug, Clone)]
pub enum AnnotationScope {
    File,
    Lines {
        start_line_idx: usize,
        end_line_idx: usize,
        start_ref: LineReference,
        end_ref: LineReference,
    },
}

impl Annotation {
    pub fn created_for_lines(
        id: u64,
        file_path: String,
        edge: Option<ReviewEdge>,
        hunk_header: Option<String>,
        range: AnnotationLineRange,
        body: String,
    ) -> Self {
        Self {
            id,
            file_path,
            edge,
            hunk_header,
            scope: AnnotationScope::Lines {
                start_line_idx: range.start_line_idx,
                end_line_idx: range.end_line_idx,
                start_ref: range.start_ref,
                end_ref: range.end_ref,
            },
            body,
        }
    }

    pub fn created_for_file(
        id: u64,
        file_path: String,
        edge: Option<ReviewEdge>,
        body: String,
    ) -> Self {
        Self {
            id,
            file_path,
            edge,
            hunk_header: None,
            scope: AnnotationScope::File,
            body,
        }
    }

    pub fn line_range(&self) -> Option<(usize, usize)> {
        match self.scope {
            AnnotationScope::File => None,
            AnnotationScope::Lines {
                start_line_idx,
                end_line_idx,
                ..
            } => Some((start_line_idx, end_line_idx)),
        }
    }

    pub fn line_refs(&self) -> Option<(&LineReference, &LineReference)> {
        match &self.scope {
            AnnotationScope::File => None,
            AnnotationScope::Lines {
                start_ref, end_ref, ..
            } => Some((start_ref, end_ref)),
        }
    }

    pub fn is_file_level(&self) -> bool {
        matches!(self.scope, AnnotationScope::File)
    }
}

#[derive(Debug, Clone)]
pub struct AnnotationLineRange {
    pub start_line_idx: usize,
    pub end_line_idx: usize,
    pub start_ref: LineReference,
    pub end_ref: LineReference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Files,
    Diff,
}

#[derive(Debug, Clone, Copy)]
pub struct SelectionRange {
    pub anchor: usize,
    pub cursor: usize,
    pub locked: bool,
}

impl SelectionRange {
    pub fn normalized(self) -> (usize, usize) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
}
