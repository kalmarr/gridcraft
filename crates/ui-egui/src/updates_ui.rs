//! One background check at startup (and on Help ▸ Check for Updates…) and the modal prompt that
//! offers the new release: Update and Restart (direct self-update), Download or Later. The flow
//! itself is `craft_update::updater::Updater`.

use craft_update::CheckResultResponse;
use craft_update::updater::{Action, Status, Updater};

use crate::SheetApp;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) struct Runtime {
    updater: Updater,
    /// The check at start was asked for; `tick` starts it with the context.
    startup: bool,
    /// Help ▸ Check for Updates… was chosen; the next `tick` starts it.
    manual: bool,
    /// The result dialog of a requested check is showing.
    result_open: bool,
}

impl Default for Runtime {
    fn default() -> Self {
        Self { updater: Updater::new(&crate::UPDATE_APP, VERSION), startup: false, manual: false, result_open: false }
    }
}

/// Ask for a check; the next frame's `tick` starts it. A manual one reports every outcome.
pub(crate) fn start(app: &mut SheetApp, manual: bool) {
    if manual {
        app.updates_rt.manual = true;
    } else {
        app.updates_rt.startup = true;
    }
}

pub(crate) fn tick(app: &mut SheetApp, ctx: &egui::Context) {
    let rt = &mut app.updates_rt;
    if std::mem::take(&mut rt.manual) {
        rt.updater.check_now(ctx);
        rt.result_open = true;
    } else if rt.startup {
        rt.updater.check_at_start(ctx);
    }
    rt.updater.tick(ctx);
    // A requested check that found a release leaves the status idle and the prompt opens.
    if matches!(rt.updater.status(), Status::Idle) {
        rt.result_open = false;
    }
}

/// Recomputed on every frame: the updater checks it before starting installation and again
/// before returning Close, so work begun during its background download also holds back restart.
fn can_restart(app: &SheetApp) -> bool {
    !app.session.documents().iter().any(|d| d.is_dirty())
        && app.editor.is_none()
        && app.text_box_editor.is_none()
        && app.parked_window_with_draft().is_none()
        && app.macros.editor.is_none()
        && app.macros.question.is_none()
        && !app.session.is_recording()
        && !app.printing.busy()
}

fn handle_action(app: &mut SheetApp, ctx: &egui::Context, action: Action) {
    match action {
        Action::OpenUrl(url) => {
            if let Some(open) = &app.services.open_url {
                open(&url);
            }
        }
        // A raw root-window Close only closes that workbook in native multi-window mode.
        // Quit the whole application through its draft/save guards before the helper relaunches.
        Action::Close => app.request_quit(ctx),
        Action::None => {}
    }
}

pub(crate) fn show(app: &mut SheetApp, ctx: &egui::Context) {
    let clean = can_restart(app);
    let action = app.updates_rt.updater.show_if(ctx, clean);
    handle_action(app, ctx, action);
    if app.updates_rt.result_open {
        let rt = &mut app.updates_rt;
        match craft_update::CheckResultDialog::new(&crate::UPDATE_APP, VERSION).show(ctx, rt.updater.status()) {
            CheckResultResponse::Close => {
                rt.updater.dismiss_status();
                rt.result_open = false;
            }
            CheckResultResponse::CheckAgain => rt.updater.check_now(ctx),
            CheckResultResponse::None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridcraft_engine::{Session, core::CellRef};
    use serde_json::json;

    fn app() -> SheetApp {
        let mut session = Session::new();
        session.new_workbook();
        let mut app = SheetApp::new(session, Default::default());
        app.follow_active_window();
        app
    }

    #[test]
    fn restart_waits_for_current_and_parked_cell_drafts_and_dirty_workbooks() {
        let mut app = app();
        assert!(can_restart(&app));
        let first = app.session.active_window().unwrap();
        let doc = app.session.active().unwrap().uid;
        app.editor = Some(crate::editor::EditState::new(doc, 0, CellRef::new(0, 0), "late draft".into(), true, false));
        assert!(!app.session.active().unwrap().is_dirty(), "a draft is not in the workbook yet");
        assert!(!can_restart(&app), "work begun during a download must block restart");
        app.session.new_workbook();
        app.follow_active_window();
        assert!(app.editor.is_none());
        assert_eq!(app.parked_window_with_draft(), Some(first));
        assert!(!can_restart(&app), "a draft in another window still blocks restart");
        app.enter_window(first);
        app.editor = None;
        assert!(can_restart(&app));
        app.run("cell.set", json!({"cell":"A1", "input":"saved later"})).unwrap();
        app.session.new_workbook();
        app.follow_active_window();
        assert!(!app.session.active().unwrap().is_dirty());
        assert!(!can_restart(&app), "an inactive dirty workbook still blocks restart");
    }

    #[test]
    fn restart_waits_for_current_and_parked_text_box_drafts() {
        let mut app = app();
        let id = app.run("insert.textBox", json!({"at":"B2", "text":"original"})).unwrap()["shape"].as_u64().unwrap() as u32;
        let doc = app.session.active_mut().unwrap();
        doc.saved = doc.wb.clone();
        assert!(app.begin_text_box_edit(id));
        app.text_box_editor.as_mut().unwrap().text = "unsaved text".into();
        assert!(!app.session.active().unwrap().is_dirty());
        assert!(!can_restart(&app));
        app.session.new_workbook();
        app.follow_active_window();
        assert!(app.text_box_editor.is_none());
        assert!(app.parked_window_with_draft().is_some());
        assert!(!can_restart(&app));
    }

    #[test]
    fn restart_waits_for_macro_work_and_print_submission() {
        let mut app = app();
        app.open_dialog("macroEditor", json!({}));
        assert!(app.macros.editor.is_some());
        assert!(!app.session.active().unwrap().is_dirty());
        assert!(!can_restart(&app), "macro drafts are not workbook edits yet");
        app.macros.editor = None;
        app.dialog = None;
        app.run("macro.record", json!({"relative":false})).unwrap();
        assert!(app.session.is_recording());
        assert!(!can_restart(&app));
        app.session.recorder = None;
        assert!(can_restart(&app));
        let (sender, receiver) = std::sync::mpsc::channel();
        app.printing.begin(receiver).unwrap();
        assert!(!can_restart(&app), "a print job started during downloading must finish first");
        sender.send(Ok("queued".into())).unwrap();
        app.poll_printing(&egui::Context::default());
        assert!(can_restart(&app));
    }

    #[test]
    fn updater_close_quits_all_native_windows_through_the_save_guards() {
        let mut app = app();
        app.session.new_workbook();
        app.follow_active_window();
        app.native_windows = true;
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| handle_action(&mut app, ui.ctx(), Action::Close));
        // This command-only frame has no renderer to consume its texture uploads.
        output.textures_delta.clear();
        assert!(app.allow_close, "native windows must recognize application-wide quit");
        assert_eq!(app.session.documents().len(), 2, "the root workbook must not close on its own");
        assert!(output.viewport_output.get(&egui::ViewportId::ROOT).unwrap().commands.contains(&egui::ViewportCommand::Close));

        // Last line of defence if state changes between the updater's guard and the close action.
        app.allow_close = false;
        app.run("cell.set", json!({"cell":"A1", "input":"late change"})).unwrap();
        ctx.run_ui(egui::RawInput::default(), |ui| handle_action(&mut app, ui.ctx(), Action::Close)).textures_delta.clear();
        assert!(!app.allow_close);
        assert_eq!(app.dialog.as_ref().unwrap().name(), "saveChanges");
    }
}
