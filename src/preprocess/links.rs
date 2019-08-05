use crate::errors::*;
use crate::utils::{take_anchored_lines, take_lines};
use regex::{CaptureMatches, Captures, Regex};
use std::fs;
use std::ops::{Bound, Range, RangeBounds, RangeFrom, RangeFull, RangeTo};
use std::path::{Path, PathBuf};

use super::{Preprocessor, PreprocessorContext};
use crate::book::{Book, BookItem};

const ESCAPE_CHAR: char = '\\';
const MAX_LINK_NESTED_DEPTH: usize = 10;

/// A preprocessor for expanding the `{{# playpen}}` and `{{# include}}`
/// helpers in a chapter.
#[derive(Default)]
pub struct LinkPreprocessor;

impl LinkPreprocessor {
    pub(crate) const NAME: &'static str = "links";

    /// Create a new `LinkPreprocessor`.
    pub fn new() -> Self {
        LinkPreprocessor
    }
}

impl Preprocessor for LinkPreprocessor {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn run(&self, ctx: &PreprocessorContext, mut book: Book) -> Result<Book> {
        let src_dir = ctx.root.join(&ctx.config.book.src);

        book.for_each_mut(|section: &mut BookItem| {
            if let BookItem::Chapter(ref mut ch) = *section {
                let base = ch
                    .path
                    .parent()
                    .map(|dir| src_dir.join(dir))
                    .expect("All book items have a parent");

                let content = replace_all(&ch.content, base, &ch.path, 0);
                ch.content = content;
            }
        });

        Ok(book)
    }
}

fn replace_all<P1, P2>(s: &str, path: P1, source: P2, depth: usize) -> String
where
    P1: AsRef<Path>,
    P2: AsRef<Path>,
{
    // When replacing one thing in a string by something with a different length,
    // the indices after that will not correspond,
    // we therefore have to store the difference to correct this
    let path = path.as_ref();
    let source = source.as_ref();
    let mut previous_end_index = 0;
    let mut replaced = String::new();

    for link in find_links(s) {
        replaced.push_str(&s[previous_end_index..link.start_index]);

        match link.render_with_path(&path) {
            Ok(new_content) => {
                if depth < MAX_LINK_NESTED_DEPTH {
                    if let Some(rel_path) = link.link_type.relative_path(path) {
                        replaced.push_str(&replace_all(&new_content, rel_path, source, depth + 1));
                    } else {
                        replaced.push_str(&new_content);
                    }
                } else {
                    error!(
                        "Stack depth exceeded in {}. Check for cyclic includes",
                        source.display()
                    );
                }
                previous_end_index = link.end_index;
            }
            Err(e) => {
                error!("Error updating \"{}\", {}", link.link_text, e);
                for cause in e.iter().skip(1) {
                    warn!("Caused By: {}", cause);
                }

                // This should make sure we include the raw `{{# ... }}` snippet
                // in the page content if there are any errors.
                previous_end_index = link.start_index;
            }
        }
    }

    replaced.push_str(&s[previous_end_index..]);
    replaced
}

#[derive(PartialEq, Debug, Clone)]
enum LinkType<'a> {
    Escaped,
    IncludeRange(PathBuf, LineRange),
    IncludeAnchor(PathBuf, String),
    Playpen(PathBuf, Vec<&'a str>),
}

// A range of lines specified with some include directive.
#[derive(PartialEq, Debug, Clone)]
enum LineRange {
    Range(Range<usize>),
    RangeFrom(RangeFrom<usize>),
    RangeTo(RangeTo<usize>),
    RangeFull(RangeFull),
}

impl RangeBounds<usize> for LineRange {
    fn start_bound(&self) -> Bound<&usize> {
        match self {
            LineRange::Range(r) => r.start_bound(),
            LineRange::RangeFrom(r) => r.start_bound(),
            LineRange::RangeTo(r) => r.start_bound(),
            LineRange::RangeFull(r) => r.start_bound(),
        }
    }

    fn end_bound(&self) -> Bound<&usize> {
        match self {
            LineRange::Range(r) => r.end_bound(),
            LineRange::RangeFrom(r) => r.end_bound(),
            LineRange::RangeTo(r) => r.end_bound(),
            LineRange::RangeFull(r) => r.end_bound(),
        }
    }
}

impl From<Range<usize>> for LineRange {
    fn from(r: Range<usize>) -> LineRange {
        LineRange::Range(r)
    }
}

impl From<RangeFrom<usize>> for LineRange {
    fn from(r: RangeFrom<usize>) -> LineRange {
        LineRange::RangeFrom(r)
    }
}

impl From<RangeTo<usize>> for LineRange {
    fn from(r: RangeTo<usize>) -> LineRange {
        LineRange::RangeTo(r)
    }
}

impl From<RangeFull> for LineRange {
    fn from(r: RangeFull) -> LineRange {
        LineRange::RangeFull(r)
    }
}

impl<'a> LinkType<'a> {
    fn relative_path<P: AsRef<Path>>(self, base: P) -> Option<PathBuf> {
        let base = base.as_ref();
        match self {
            LinkType::Escaped => None,
            LinkType::IncludeRange(p, _) => Some(return_relative_path(base, &p)),
            LinkType::IncludeAnchor(p, _) => Some(return_relative_path(base, &p)),
            LinkType::Playpen(p, _) => Some(return_relative_path(base, &p)),
        }
    }
}
fn return_relative_path<P: AsRef<Path>>(base: P, relative: P) -> PathBuf {
    base.as_ref()
        .join(relative)
        .parent()
        .expect("Included file should not be /")
        .to_path_buf()
}

fn parse_include_path(path: &str) -> LinkType<'static> {
    let mut parts = path.split(':').fuse();
    let path = parts.next().unwrap().into();

    let next_element = parts.next();
    let start = if let Some(value) = next_element.and_then(|s| s.parse::<usize>().ok()) {
        // subtract 1 since line numbers usually begin with 1
        Some(value.saturating_sub(1))
    } else if let Some(anchor) = next_element {
        if anchor == "" {
            None
        } else {
            return LinkType::IncludeAnchor(path, String::from(anchor));
        }
    } else {
        None
    };

    let end = parts.next();
    let has_end = end.is_some();
    let end = end.and_then(|s| s.parse::<usize>().ok());
    match start {
        Some(start) => match end {
            Some(end) => LinkType::IncludeRange(path, LineRange::from(start..end)),
            None => {
                if has_end {
                    LinkType::IncludeRange(path, LineRange::from(start..))
                } else {
                    LinkType::IncludeRange(
                        path,
                        LineRange::from(start..start + 1),
                    )
                }
            }
        },
        None => match end {
            Some(end) => LinkType::IncludeRange(path, LineRange::from(..end)),
            None => LinkType::IncludeRange(path, LineRange::from(RangeFull)),
        },
    }
}

#[derive(PartialEq, Debug, Clone)]
struct Link<'a> {
    start_index: usize,
    end_index: usize,
    link_type: LinkType<'a>,
    link_text: &'a str,
}

impl<'a> Link<'a> {
    fn from_capture(cap: Captures<'a>) -> Option<Link<'a>> {
        let link_type = match (cap.get(0), cap.get(1), cap.get(2)) {
            (_, Some(typ), Some(rest)) => {
                let mut path_props = rest.as_str().split_whitespace();
                let file_arg = path_props.next();
                let props: Vec<&str> = path_props.collect();

                match (typ.as_str(), file_arg) {
                    ("include", Some(pth)) => Some(parse_include_path(pth)),
                    ("playpen", Some(pth)) => Some(LinkType::Playpen(pth.into(), props)),
                    _ => None,
                }
            }
            (Some(mat), None, None) if mat.as_str().starts_with(ESCAPE_CHAR) => {
                Some(LinkType::Escaped)
            }
            _ => None,
        };

        link_type.and_then(|lnk_type| {
            cap.get(0).map(|mat| Link {
                start_index: mat.start(),
                end_index: mat.end(),
                link_type: lnk_type,
                link_text: mat.as_str(),
            })
        })
    }

    fn render_with_path<P: AsRef<Path>>(&self, base: P) -> Result<String> {
        let base = base.as_ref();
        match self.link_type {
            // omit the escape char
            LinkType::Escaped => Ok((&self.link_text[1..]).to_owned()),
            LinkType::IncludeRange(ref pat, ref range) => {
                let target = base.join(pat);

                fs::read_to_string(&target)
                    .map(|s| take_lines(&s, range.clone()))
                    .chain_err(|| {
                        format!(
                            "Could not read file for link {} ({})",
                            self.link_text,
                            target.display(),
                        )
                    })
            }
            LinkType::IncludeAnchor(ref pat, ref anchor) => {
                let target = base.join(pat);

                fs::read_to_string(&target)
                    .map(|s| take_anchored_lines(&s, anchor))
                    .chain_err(|| {
                        format!(
                            "Could not read file for link {} ({})",
                            self.link_text,
                            target.display(),
                        )
                    })
            }
            LinkType::Playpen(ref pat, ref attrs) => {
                let target = base.join(pat);

                let contents = fs::read_to_string(&target).chain_err(|| {
                    format!(
                        "Could not read file for link {} ({})",
                        self.link_text,
                        target.display()
                    )
                })?;
                let ftype = if !attrs.is_empty() { "rust," } else { "rust" };
                Ok(format!(
                    "```{}{}\n{}\n```\n",
                    ftype,
                    attrs.join(","),
                    contents
                ))
            }
        }
    }
}

struct LinkIter<'a>(CaptureMatches<'a, 'a>);

impl<'a> Iterator for LinkIter<'a> {
    type Item = Link<'a>;
    fn next(&mut self) -> Option<Link<'a>> {
        for cap in &mut self.0 {
            if let Some(inc) = Link::from_capture(cap) {
                return Some(inc);
            }
        }
        None
    }
}

fn find_links(contents: &str) -> LinkIter<'_> {
    // lazily compute following regex
    // r"\\\{\{#.*\}\}|\{\{#([a-zA-Z0-9]+)\s*([a-zA-Z0-9_.\-:/\\\s]+)\}\}")?;
    lazy_static! {
        static ref RE: Regex = Regex::new(
            r"(?x)                     # insignificant whitespace mode
            \\\{\{\#.*\}\}             # match escaped link
            |                          # or
            \{\{\s*                    # link opening parens and whitespace
            \#([a-zA-Z0-9]+)           # link type
            \s+                        # separating whitespace
            ([a-zA-Z0-9\s_.\-:/\\]+)   # link target path and space separated properties
            \s*\}\}                    # whitespace and link closing parens"
        )
        .unwrap();
    }
    LinkIter(RE.captures_iter(contents))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_all_escaped() {
        let start = r"
        Some text over here.
        ```hbs
        \{{#include file.rs}} << an escaped link!
        ```";
        let end = r"
        Some text over here.
        ```hbs
        {{#include file.rs}} << an escaped link!
        ```";
        assert_eq!(replace_all(start, "", "", 0), end);
    }

    #[test]
    fn test_find_links_no_link() {
        let s = "Some random text without link...";
        assert!(find_links(s).collect::<Vec<_>>() == vec![]);
    }

    #[test]
    fn test_find_links_partial_link() {
        let s = "Some random text with {{#playpen...";
        assert!(find_links(s).collect::<Vec<_>>() == vec![]);
        let s = "Some random text with {{#include...";
        assert!(find_links(s).collect::<Vec<_>>() == vec![]);
        let s = "Some random text with \\{{#include...";
        assert!(find_links(s).collect::<Vec<_>>() == vec![]);
    }

    #[test]
    fn test_find_links_empty_link() {
        let s = "Some random text with {{#playpen}} and {{#playpen   }} {{}} {{#}}...";
        assert!(find_links(s).collect::<Vec<_>>() == vec![]);
    }

    #[test]
    fn test_find_links_unknown_link_type() {
        let s = "Some random text with {{#playpenz ar.rs}} and {{#incn}} {{baz}} {{#bar}}...";
        assert!(find_links(s).collect::<Vec<_>>() == vec![]);
    }

    #[test]
    fn test_find_links_simple_link() {
        let s = "Some random text with {{#playpen file.rs}} and {{#playpen test.rs }}...";

        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {:?}\n", res);

        assert_eq!(
            res,
            vec![
                Link {
                    start_index: 22,
                    end_index: 42,
                    link_type: LinkType::Playpen(PathBuf::from("file.rs"), vec![]),
                    link_text: "{{#playpen file.rs}}",
                },
                Link {
                    start_index: 47,
                    end_index: 68,
                    link_type: LinkType::Playpen(PathBuf::from("test.rs"), vec![]),
                    link_text: "{{#playpen test.rs }}",
                },
            ]
        );
    }

    #[test]
    fn test_find_links_with_range() {
        let s = "Some random text with {{#include file.rs:10:20}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {:?}\n", res);
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 48,
                link_type: LinkType::IncludeRange(PathBuf::from("file.rs"), LineRange::from(9..20)),
                link_text: "{{#include file.rs:10:20}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_line_number() {
        let s = "Some random text with {{#include file.rs:10}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {:?}\n", res);
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 45,
                link_type: LinkType::IncludeRange(PathBuf::from("file.rs"), LineRange::from(9..10)),
                link_text: "{{#include file.rs:10}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_from_range() {
        let s = "Some random text with {{#include file.rs:10:}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {:?}\n", res);
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 46,
                link_type: LinkType::IncludeRange(PathBuf::from("file.rs"), LineRange::from(9..)),
                link_text: "{{#include file.rs:10:}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_to_range() {
        let s = "Some random text with {{#include file.rs::20}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {:?}\n", res);
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 46,
                link_type: LinkType::IncludeRange(PathBuf::from("file.rs"), LineRange::from(..20)),
                link_text: "{{#include file.rs::20}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_full_range() {
        let s = "Some random text with {{#include file.rs::}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {:?}\n", res);
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 44,
                link_type: LinkType::IncludeRange(PathBuf::from("file.rs"), LineRange::from(..)),
                link_text: "{{#include file.rs::}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_no_range_specified() {
        let s = "Some random text with {{#include file.rs}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {:?}\n", res);
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 42,
                link_type: LinkType::IncludeRange(PathBuf::from("file.rs"), LineRange::from(..)),
                link_text: "{{#include file.rs}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_anchor() {
        let s = "Some random text with {{#include file.rs:anchor}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {:?}\n", res);
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 49,
                link_type: LinkType::IncludeAnchor(
                    PathBuf::from("file.rs"),
                    String::from("anchor")
                ),
                link_text: "{{#include file.rs:anchor}}",
            }]
        );
    }

    #[test]
    fn test_find_links_escaped_link() {
        let s = "Some random text with escaped playpen \\{{#playpen file.rs editable}} ...";

        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {:?}\n", res);

        assert_eq!(
            res,
            vec![Link {
                start_index: 38,
                end_index: 68,
                link_type: LinkType::Escaped,
                link_text: "\\{{#playpen file.rs editable}}",
            }]
        );
    }

    #[test]
    fn test_find_playpens_with_properties() {
        let s = "Some random text with escaped playpen {{#playpen file.rs editable }} and some \
                 more\n text {{#playpen my.rs editable no_run should_panic}} ...";

        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {:?}\n", res);
        assert_eq!(
            res,
            vec![
                Link {
                    start_index: 38,
                    end_index: 68,
                    link_type: LinkType::Playpen(PathBuf::from("file.rs"), vec!["editable"]),
                    link_text: "{{#playpen file.rs editable }}",
                },
                Link {
                    start_index: 89,
                    end_index: 136,
                    link_type: LinkType::Playpen(
                        PathBuf::from("my.rs"),
                        vec!["editable", "no_run", "should_panic"],
                    ),
                    link_text: "{{#playpen my.rs editable no_run should_panic}}",
                },
            ]
        );
    }

    #[test]
    fn test_find_all_link_types() {
        let s = "Some random text with escaped playpen {{#include file.rs}} and \\{{#contents are \
                 insignifficant in escaped link}} some more\n text  {{#playpen my.rs editable \
                 no_run should_panic}} ...";

        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {:?}\n", res);
        assert_eq!(res.len(), 3);
        assert_eq!(
            res[0],
            Link {
                start_index: 38,
                end_index: 58,
                link_type: LinkType::IncludeRange(PathBuf::from("file.rs"), LineRange::from(..)),
                link_text: "{{#include file.rs}}",
            }
        );
        assert_eq!(
            res[1],
            Link {
                start_index: 63,
                end_index: 112,
                link_type: LinkType::Escaped,
                link_text: "\\{{#contents are insignifficant in escaped link}}",
            }
        );
        assert_eq!(
            res[2],
            Link {
                start_index: 130,
                end_index: 177,
                link_type: LinkType::Playpen(
                    PathBuf::from("my.rs"),
                    vec!["editable", "no_run", "should_panic"]
                ),
                link_text: "{{#playpen my.rs editable no_run should_panic}}",
            }
        );
    }

    #[test]
    fn parse_without_colon_includes_all() {
        let link_type = parse_include_path("arbitrary");
        assert_eq!(
            link_type,
            LinkType::IncludeRange(PathBuf::from("arbitrary"), LineRange::from(RangeFull))
        );
    }

    #[test]
    fn parse_with_nothing_after_colon_includes_all() {
        let link_type = parse_include_path("arbitrary:");
        assert_eq!(
            link_type,
            LinkType::IncludeRange(PathBuf::from("arbitrary"), LineRange::from(RangeFull))
        );
    }

    #[test]
    fn parse_with_two_colons_includes_all() {
        let link_type = parse_include_path("arbitrary::");
        assert_eq!(
            link_type,
            LinkType::IncludeRange(PathBuf::from("arbitrary"), LineRange::from(RangeFull))
        );
    }

    #[test]
    fn parse_with_garbage_after_two_colons_includes_all() {
        let link_type = parse_include_path("arbitrary::NaN");
        assert_eq!(
            link_type,
            LinkType::IncludeRange(PathBuf::from("arbitrary"), LineRange::from(RangeFull))
        );
    }

    #[test]
    fn parse_with_one_number_after_colon_only_that_line() {
        let link_type = parse_include_path("arbitrary:5");
        assert_eq!(
            link_type,
            LinkType::IncludeRange(PathBuf::from("arbitrary"), LineRange::from(4..5))
        );
    }

    #[test]
    fn parse_with_one_based_start_becomes_zero_based() {
        let link_type = parse_include_path("arbitrary:1");
        assert_eq!(
            link_type,
            LinkType::IncludeRange(PathBuf::from("arbitrary"), LineRange::from(0..1))
        );
    }

    #[test]
    fn parse_with_zero_based_start_stays_zero_based_but_is_probably_an_error() {
        let link_type = parse_include_path("arbitrary:0");
        assert_eq!(
            link_type,
            LinkType::IncludeRange(PathBuf::from("arbitrary"), LineRange::from(0..1))
        );
    }

    #[test]
    fn parse_start_only_range() {
        let link_type = parse_include_path("arbitrary:5:");
        assert_eq!(
            link_type,
            LinkType::IncludeRange(PathBuf::from("arbitrary"), LineRange::from(4..))
        );
    }

    #[test]
    fn parse_start_with_garbage_interpreted_as_start_only_range() {
        let link_type = parse_include_path("arbitrary:5:NaN");
        assert_eq!(
            link_type,
            LinkType::IncludeRange(PathBuf::from("arbitrary"), LineRange::from(4..))
        );
    }

    #[test]
    fn parse_end_only_range() {
        let link_type = parse_include_path("arbitrary::5");
        assert_eq!(
            link_type,
            LinkType::IncludeRange(PathBuf::from("arbitrary"), LineRange::from(..5))
        );
    }

    #[test]
    fn parse_start_and_end_range() {
        let link_type = parse_include_path("arbitrary:5:10");
        assert_eq!(
            link_type,
            LinkType::IncludeRange(PathBuf::from("arbitrary"), LineRange::from(4..10))
        );
    }

    #[test]
    fn parse_with_negative_interpreted_as_anchor() {
        let link_type = parse_include_path("arbitrary:-5");
        assert_eq!(
            link_type,
            LinkType::IncludeAnchor(PathBuf::from("arbitrary"), "-5".to_string())
        );
    }

    #[test]
    fn parse_with_floating_point_interpreted_as_anchor() {
        let link_type = parse_include_path("arbitrary:-5.7");
        assert_eq!(
            link_type,
            LinkType::IncludeAnchor(PathBuf::from("arbitrary"), "-5.7".to_string())
        );
    }

    #[test]
    fn parse_with_anchor_followed_by_colon() {
        let link_type = parse_include_path("arbitrary:some-anchor:this-gets-ignored");
        assert_eq!(
            link_type,
            LinkType::IncludeAnchor(PathBuf::from("arbitrary"), "some-anchor".to_string())
        );
    }

    #[test]
    fn parse_with_more_than_three_colons_ignores_everything_after_third_colon() {
        let link_type = parse_include_path("arbitrary:5:10:17:anything:");
        assert_eq!(
            link_type,
            LinkType::IncludeRange(PathBuf::from("arbitrary"), LineRange::from(4..10))
        );
    }
}
