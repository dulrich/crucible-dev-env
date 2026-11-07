//! Tests for the picker overlay.

use super::*;

fn cand(kind: CandidateKind, name: &str, path: Option<&str>) -> Candidate {
    Candidate {
        kind,
        name: name.into(),
        path: path.map(PathBuf::from),
        slot: None,
    }
}

#[test]
fn table_has_a_header_and_the_slot_column_only_when_a_row_has_a_slot() {
    let mut p = sample();
    let (h, t) = p.table();
    assert_eq!(h.as_deref(), Some("kind    name   root"));
    assert_eq!(t[0], "live    alpha");
    assert_eq!(t[2], "folder  proj   /home/u/proj");
    p.candidates[0].slot = Some(3);
    let (h, t) = p.table();
    assert_eq!(h.as_deref(), Some("kind    slot  name   root"));
    assert_eq!(t[0], "live    \u{2318}3    alpha");
    assert_eq!(t[1], "saved         beta");
    // The new-workspace row is unpadded and carries no header on its own.
    type_str(&mut p, "zz");
    let (h, t) = p.table();
    assert_eq!((h, t), (None, vec!["new workspace zz".to_string()]));
}

fn select(p: &mut Picker, name: &str) {
    for _ in 0..4 {
        p.step(Key::Up);
    }
    while p
        .rows()
        .get(p.list.selected)
        .is_some_and(|r| !matches!(r, Row::Candidate(i) if p.candidates[*i].name == name))
    {
        p.step(Key::Down);
    }
}

#[test]
fn delete_on_a_saved_row_needs_two_presses() {
    let mut p = sample();
    select(&mut p, "beta");
    assert_eq!(p.step(Key::Delete), Outcome::Continue);
    assert!(p.status().unwrap().contains("Delete again"));
    assert_eq!(
        p.step(Key::Delete),
        Outcome::Send(Request::Delete("beta".into()))
    );
    // Disarmed after the send: the next press arms again.
    assert_eq!(p.step(Key::Delete), Outcome::Continue);
}

#[test]
fn any_other_key_disarms_the_delete_confirm() {
    let mut p = sample();
    select(&mut p, "beta");
    p.step(Key::Delete);
    // Moving away and back, or typing, disarms; the key is still handled normally.
    p.step(Key::Up);
    assert_eq!(p.status(), None);
    p.step(Key::Down);
    assert_eq!(p.step(Key::Delete), Outcome::Continue);
    p.step(Key::Char('b'));
    assert_eq!(p.query(), "b");
    assert_eq!(p.step(Key::Delete), Outcome::Continue);
    assert!(p.status().unwrap().contains("Delete again"));
}

#[test]
fn delete_on_a_different_saved_row_re_arms_instead_of_sending() {
    let mut p = Picker::new(
        vec![
            cand(CandidateKind::Saved, "one", None),
            cand(CandidateKind::Saved, "two", None),
        ],
        None,
    );
    select(&mut p, "one");
    p.step(Key::Delete);
    // Down is "any other key": it disarms, so Delete on "two" only arms.
    p.step(Key::Down);
    assert_eq!(p.step(Key::Delete), Outcome::Continue);
    assert_eq!(
        p.step(Key::Delete),
        Outcome::Send(Request::Delete("two".into()))
    );
}

#[test]
fn delete_on_live_folder_and_new_rows_sends_nothing() {
    let mut p = sample();
    select(&mut p, "alpha");
    assert_eq!(p.step(Key::Delete), Outcome::Continue);
    assert_eq!(p.status(), Some("open workspace: use delete-workspace"));
    assert_eq!(p.step(Key::Delete), Outcome::Continue);
    select(&mut p, "proj");
    assert_eq!(p.step(Key::Delete), Outcome::Continue);
    assert_eq!(p.step(Key::Delete), Outcome::Continue);
    let mut p = sample();
    type_str(&mut p, "zzz");
    assert_eq!(p.step(Key::Delete), Outcome::Continue);
    assert_eq!(p.step(Key::Delete), Outcome::Continue);
    // Root mode never deletes.
    let mut r = Picker::root(
        "",
        vec![cand(CandidateKind::Folder, "proj", Some("/p"))],
        None,
    );
    assert_eq!(r.step(Key::Delete), Outcome::Continue);
}

#[test]
fn rename_mode_has_no_header() {
    let p = RenamePrompt::new("x".into());
    assert_eq!(
        p.render(40, 6),
        "\x1b[H\x1b[2J\r\n  rename> x_\r\n\r\n\r\n\r\n"
    );
}

fn sample() -> Picker {
    Picker::new(
        vec![
            cand(CandidateKind::Live, "alpha", None),
            cand(CandidateKind::Saved, "beta", None),
            cand(CandidateKind::Folder, "proj", Some("/home/u/proj")),
        ],
        None,
    )
}

fn type_str(p: &mut Picker, s: &str) {
    for c in s.chars() {
        p.step(Key::Char(c));
    }
}

#[test]
fn enter_opens_live_saved_and_folder() {
    let mut p = sample();
    assert_eq!(
        p.step(Key::Enter),
        Outcome::Send(Request::Open("alpha".into()))
    );
    p.step(Key::Down);
    assert_eq!(
        p.step(Key::Enter),
        Outcome::Send(Request::Open("beta".into()))
    );
    p.step(Key::Down);
    assert_eq!(
        p.step(Key::Enter),
        Outcome::Send(Request::OpenFolder(PathBuf::from("/home/u/proj")))
    );
}

#[test]
fn selection_is_clamped() {
    let mut p = sample();
    p.step(Key::Up);
    assert_eq!(
        p.step(Key::Enter),
        Outcome::Send(Request::Open("alpha".into()))
    );
    for _ in 0..10 {
        p.step(Key::CtrlN);
    }
    assert!(matches!(
        p.step(Key::Enter),
        Outcome::Send(Request::OpenFolder(_))
    ));
    p.step(Key::CtrlP);
    assert_eq!(
        p.step(Key::Enter),
        Outcome::Send(Request::Open("beta".into()))
    );
}

#[test]
fn filtering_narrows_rows_and_appends_new_row() {
    let mut p = sample();
    type_str(&mut p, "bet");
    assert_eq!(p.rows(), vec![Row::Candidate(1), Row::New("bet".into())]);
    assert_eq!(
        p.step(Key::Enter),
        Outcome::Send(Request::Open("beta".into()))
    );
}

#[test]
fn exact_workspace_name_has_no_new_row_but_folder_name_does() {
    let mut p = sample();
    type_str(&mut p, "beta");
    assert_eq!(p.rows(), vec![Row::Candidate(1)]);
    let mut q = sample();
    type_str(&mut q, "proj");
    assert_eq!(q.rows(), vec![Row::Candidate(2), Row::New("proj".into())]);
}

#[test]
fn no_match_creates_new_workspace() {
    let mut p = sample();
    type_str(&mut p, "zzz");
    assert_eq!(p.rows(), vec![Row::New("zzz".into())]);
    assert_eq!(
        p.step(Key::Enter),
        Outcome::Send(Request::New("zzz".into()))
    );
}

#[test]
fn new_row_can_be_selected_explicitly() {
    let mut p = sample();
    type_str(&mut p, "a");
    let n = p.rows().len();
    for _ in 0..n {
        p.step(Key::Down);
    }
    assert_eq!(p.step(Key::Enter), Outcome::Send(Request::New("a".into())));
}

#[test]
fn empty_query_and_no_candidates_enter_is_noop() {
    let mut p = Picker::new(vec![], None);
    assert_eq!(p.step(Key::Enter), Outcome::Continue);
}

#[test]
fn backspace_edits_and_resets_selection() {
    let mut p = sample();
    type_str(&mut p, "ab");
    p.step(Key::Backspace);
    assert_eq!(p.query(), "a");
    p.step(Key::Backspace);
    p.step(Key::Backspace);
    assert_eq!(p.query(), "");
}

#[test]
fn exit_keys() {
    assert_eq!(sample().step(Key::Esc), Outcome::Exit);
    assert_eq!(sample().step(Key::CtrlC), Outcome::Exit);
    assert_eq!(sample().step(Key::CtrlD), Outcome::Exit);
    let mut p = sample();
    p.step(Key::Char('a'));
    assert_eq!(p.step(Key::CtrlD), Outcome::Continue);
    assert_eq!(p.step(Key::CtrlC), Outcome::Exit);
}

#[test]
fn response_ok_exits_err_stays_with_status() {
    let mut p = sample();
    assert_eq!(p.on_response(&Response::Ok(String::new())), Outcome::Exit);
    assert_eq!(
        p.on_response(&Response::Err("already open".into())),
        Outcome::Continue
    );
    assert_eq!(p.status(), Some("already open"));
    assert!(p.render(40, 10).contains("already open"));
    p.step(Key::Char('x'));
    assert_eq!(p.status(), None);
}

#[test]
fn rename_prompt_prefills_and_sends() {
    let mut p = RenamePrompt::new("old name".into());
    assert_eq!(p.list.query, "old name");
    assert_eq!(p.row_count(), 0);
    assert_eq!(
        p.step(Key::Enter),
        Outcome::Send(Request::Rename("old name".into()))
    );
    p.step(Key::Char('!'));
    assert_eq!(
        p.step(Key::Enter),
        Outcome::Send(Request::Rename("old name!".into()))
    );
}

#[test]
fn rename_empty_is_rejected_locally() {
    let mut p = RenamePrompt::new("a".into());
    p.step(Key::Backspace);
    assert_eq!(p.step(Key::Enter), Outcome::Continue);
    assert_eq!(p.list.status.as_deref(), Some("name required"));
    assert_eq!(p.step(Key::Esc), Outcome::Exit);
}

#[test]
fn render_styled_n1_paints_background_and_reapplies_after_highlight() {
    let st = Style::parse(Some("n1-dark"));
    let out = sample().render_styled(40, 8, &st);
    assert!(out.starts_with(&format!("\x1b[H{}\x1b[2J", st.base())));
    assert!(out.contains(&st.highlight_start()));
    assert!(out.contains(&format!("\x1b[0m{}", st.base())));
    assert!(!out.contains("\x1b[7m"));
    assert_eq!(
        sample().render_styled(40, 8, &Style::Terminal),
        sample().render(40, 8)
    );
}
#[test]
fn render_shows_kinds_paths_and_marks_selection() {
    let p = sample();
    let s = p.render(60, 12);
    assert!(s.contains("workspace> _"));
    assert!(s.contains("  kind    name   root"));
    assert!(s.contains("live    alpha"));
    assert!(s.contains("saved   beta"));
    assert!(s.contains("folder  proj   /home/u/proj"));
    assert!(s.contains("\x1b[7m  live    alpha"));
    assert!(!s.contains("\x1b[7m  kind"));
    let mut q = sample();
    type_str(&mut q, "zz");
    assert!(q.render(60, 12).contains("new workspace zz"));
}

#[test]
fn render_neutralises_control_characters_and_truncates() {
    let p = Picker::new(vec![cand(CandidateKind::Live, "a\x1b[31mb", None)], None);
    let s = p.render(60, 8);
    assert!(!s.contains("a\x1b[31m"));
    assert!(s.contains("a?[31mb"));
    let long = Picker::new(
        vec![cand(CandidateKind::Live, &"x".repeat(200), None)],
        None,
    );
    assert!(long.render(20, 8).lines().all(|l| l.chars().count() < 40));
}
#[test]
fn args_parse_and_decode() {
    assert_eq!(parse_args(&[]).unwrap(), Parsed::Run(PickTarget::Workspace));
    assert_eq!(
        parse_args(&["--rename".into(), "a%3Ab%25".into()]).unwrap(),
        Parsed::Run(PickTarget::Rename("a:b%".into()))
    );
    assert_eq!(
        parse_args(&["--root".into(), "/a%3Ab".into()]).unwrap(),
        Parsed::Run(PickTarget::Root("/a:b".into()))
    );
    assert_eq!(
        parse_args(&["--root".into(), String::new()]).unwrap(),
        Parsed::Run(PickTarget::Root(String::new()))
    );
    assert!(parse_args(&["--root".into()]).is_err());
    let both: Vec<String> = ["--rename", "a", "--root", "b"].map(String::from).to_vec();
    assert!(
        parse_args(&both)
            .unwrap_err()
            .contains("cannot be combined")
    );
    assert!(parse_args(&["--rename".into(), "bad%zz".into()]).is_err());
    assert!(parse_args(&["--rename".into()]).is_err());
    assert!(parse_args(&["--nope".into()]).is_err());
}

#[test]
fn help_never_eats_the_rename_value() {
    let a = |l: &[&str]| l.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    assert_eq!(parse_args(&a(&["--help"])), Ok(Parsed::Help));
    assert_eq!(parse_args(&a(&["-h"])), Ok(Parsed::Help));
    // `--help` as the rename value is payload (and an invalid escape-free name).
    assert_eq!(
        parse_args(&a(&["--rename", "--help"])),
        Ok(Parsed::Run(PickTarget::Rename("--help".into())))
    );
    assert_eq!(
        parse_args(&a(&["--root", "--help"])),
        Ok(Parsed::Run(PickTarget::Root("--help".into())))
    );
}

fn root_picker(current: &str) -> Picker {
    Picker::root(
        current,
        vec![
            cand(CandidateKind::Live, "alpha", Some("/live")),
            cand(CandidateKind::Folder, "proj", Some("/home/u/proj")),
            cand(CandidateKind::Folder, "other", Some("/srv/other")),
        ],
        Some("/home/u".into()),
    )
}

#[test]
fn root_prompt_prefills_with_tilde_and_lists_only_folders() {
    let p = root_picker("/home/u/code");
    assert_eq!(p.query(), "~/code");
    assert_eq!(root_picker("/home/user").query(), "/home/user");
    assert_eq!(root_picker("/home/u").query(), "~");
    assert_eq!(root_picker("").query(), "");
    assert_eq!(
        root_picker("").rows(),
        vec![Row::Candidate(0), Row::Candidate(1)]
    );
    assert!(root_picker("").render(60, 8).contains("root> _"));
    assert_eq!(
        expand_tilde("~/x", Some("/home/u/")).as_deref(),
        Some("/home/u/x")
    );
    assert_eq!(expand_tilde("~", None), None);
    assert_eq!(
        expand_tilde("~/", Some("/home/u")).as_deref(),
        Some("/home/u/")
    );
    assert_eq!(
        expand_tilde("~/x/", Some("/home/u")).as_deref(),
        Some("/home/u/x/")
    );
}

#[test]
fn root_enter_precedence_empty_typed_row_hint() {
    // (a) empty query clears, even with a folder row selected.
    let mut p = root_picker("");
    assert_eq!(p.step(Key::Enter), Outcome::Send(Request::SetRoot(None)));
    // (b) the prefilled `~/x` sends itself, expanded, over the selected row.
    let mut p = root_picker("/home/u/x");
    assert_eq!(
        p.step(Key::Enter),
        Outcome::Send(Request::SetRoot(Some("/home/u/x".into())))
    );
    type_str(&mut p, "/y");
    assert_eq!(
        p.step(Key::Enter),
        Outcome::Send(Request::SetRoot(Some("/home/u/x/y".into())))
    );
    // (c) a non-path query selects the filtered folder row.
    let mut p = root_picker("");
    type_str(&mut p, "oth");
    assert_eq!(
        p.step(Key::Enter),
        Outcome::Send(Request::SetRoot(Some("/srv/other".into())))
    );
    // (d) nothing matches: hint, stay open.
    let mut p = root_picker("");
    type_str(&mut p, "zzz");
    assert_eq!(p.step(Key::Enter), Outcome::Continue);
    assert_eq!(p.status(), Some("type a path or pick a folder"));
}

#[test]
fn live_and_saved_rows_show_their_root_with_tilde() {
    let p = Picker::new(
        vec![
            cand(CandidateKind::Live, "alpha", Some("/home/u/code")),
            cand(CandidateKind::Saved, "beta", Some("/srv/b")),
            cand(CandidateKind::Saved, "gamma", None),
        ],
        Some("/home/u".into()),
    );
    let s = p.render(60, 8);
    assert!(s.contains("live   alpha  ~/code"));
    assert!(s.contains("saved  beta   /srv/b"));
    assert!(s.contains("saved  gamma\r\n"));
}
