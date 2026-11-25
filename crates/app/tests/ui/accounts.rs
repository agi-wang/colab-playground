//! Accounts: adding (code, wrong code, cancel), switching with its impact, removing, the
//! keyring state, over the headless mock.

use colab_playground::{
    i18n::keys,
    route::Route,
    state::{AccountDialog, AddPhase, Load, RemoveBlock, SignInStatus, sign_in_status},
    ui::pages::accounts::{ADD_SELECTOR, remove_selector, row_selector, switch_selector},
};
use colab_playground_core::{
    ErrorKind,
    accounts::AccountsView,
    host::ProbeReport,
    mock::{MockAccount, MockFailures, MockLoginFailure, MockScenario},
};
use gpui::TestAppContext;

use crate::support::{Options, TestApp};

fn on_accounts(app: &mut TestApp<'_>) {
    app.click(Route::Accounts.nav_selector());
    assert_eq!(app.route(), Route::Accounts);
}

fn dialog(app: &mut TestApp<'_>) -> Option<AccountDialog> {
    app.read(|state, _| state.dialog().cloned())
}

fn add_phase(app: &mut TestApp<'_>) -> Option<AddPhase> {
    match dialog(app) {
        Some(AccountDialog::Add(phase)) => Some(phase),
        _ => None,
    }
}

fn emails(app: &mut TestApp<'_>) -> Vec<String> {
    app.read(|state, _| {
        state
            .account_rows()
            .iter()
            .map(|row| row.account.email.clone().unwrap_or_default())
            .collect()
    })
}

fn active_email(app: &mut TestApp<'_>) -> Option<String> {
    app.read(|state, _| state.active_email())
}

fn paste_code(app: &mut TestApp<'_>, code: &str) {
    app.fill(|inputs| inputs.code.clone(), code);
}

/// Opens the dialog and gets as far as the code field.
fn start_adding(app: &mut TestApp<'_>) -> String {
    on_accounts(app);
    app.click(ADD_SELECTOR);
    assert_eq!(
        add_phase(app),
        Some(AddPhase::Intro),
        "nothing starts before Continue"
    );
    app.click("add-continue");
    let Some(AddPhase::Code { url }) = add_phase(app) else {
        panic!("expected the code step, got {:?}", dialog(app));
    };
    url
}

#[gpui::test]
fn adding_an_account_opens_the_browser_and_signs_in_with_the_code(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::signed_out()));
    assert_eq!(
        app.read(|state, _| state.sign_in_status()),
        SignInStatus::SignedOut
    );
    let url = start_adding(&mut app);
    assert_eq!(
        app.opened_url().as_deref(),
        Some(url.as_str()),
        "the browser opened"
    );
    assert!(app.shown("add-code-field"));
    paste_code(&mut app, "4/0AbCdEf");
    app.click("add-submit");
    assert_eq!(dialog(&mut app).map(|_| ()), None, "the dialog closes");
    assert_eq!(emails(&mut app), ["user1@example.com"]);
    assert_eq!(active_email(&mut app).as_deref(), Some("user1@example.com"));
    let signed_in = app.cx.update(|_, cx| {
        colab_playground::i18n::tr_args(cx, keys::ADD_DONE, &[("email", "user1@example.com")])
            .to_string()
    });
    assert!(
        app.toasts().iter().any(|toast| toast.as_ref() == signed_in),
        "{:?}",
        app.toasts()
    );
    assert!(app.shown(&row_selector("user1@example.com", true)));
}

#[gpui::test]
fn a_wrong_code_says_so_and_starting_again_gets_a_new_page(cx: &mut TestAppContext) {
    let options = Options {
        failures: MockFailures {
            login: Some(MockLoginFailure::InvalidCode),
            ..MockFailures::default()
        },
        ..Options::scenario(MockScenario::single())
    };
    let mut app = TestApp::with(cx, options);
    start_adding(&mut app);
    paste_code(&mut app, "not-the-code");
    app.click("add-submit");
    let Some(AddPhase::Failed { error }) = add_phase(&mut app) else {
        panic!("expected a failure, got {:?}", dialog(&mut app));
    };
    assert_eq!(error.kind, ErrorKind::LoginInvalidCode);
    assert!(app.shown("error-add"), "the error is shown in the dialog");
    assert_eq!(
        active_email(&mut app).as_deref(),
        Some("ada@example.com"),
        "rolled back"
    );

    app.set_failures(MockFailures::default());
    app.click("add-start-again");
    assert!(matches!(add_phase(&mut app), Some(AddPhase::Code { .. })));
    paste_code(&mut app, "4/0good");
    app.click("add-submit");
    assert_eq!(emails(&mut app).len(), 2);
}

#[gpui::test]
fn closing_the_dialog_rolls_back_and_changes_nothing(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    start_adding(&mut app);
    app.click("dialog-cancel");
    assert!(dialog(&mut app).is_none());
    assert_eq!(emails(&mut app), ["ada@example.com"]);
    assert_eq!(active_email(&mut app).as_deref(), Some("ada@example.com"));
    assert!(!app.read(|state, _| state.is_busy(colab_playground::state::Op::Login)));
}

#[gpui::test]
fn switching_shows_what_keeps_running_then_switches(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    on_accounts(&mut app);
    app.click(&switch_selector("lin@example.com"));
    let Some(AccountDialog::Switch { impact, .. }) = dialog(&mut app) else {
        panic!("expected the switch dialog");
    };
    let Load::Ready((impact, _)) = impact else {
        panic!("expected the impact, got {impact:?}");
    };
    let running: Vec<String> = impact
        .sessions_left_running
        .iter()
        .map(|session| session.name.clone())
        .collect();
    assert_eq!(running.len(), 2, "{running:?}");
    assert!(running.contains(&"a100-7f3a".to_owned()));
    assert!(app.shown("switch-impact"), "the dialog lists them");
    assert_eq!(
        active_email(&mut app).as_deref(),
        Some("ada@example.com"),
        "not yet"
    );

    app.click("switch-confirm");
    assert!(dialog(&mut app).is_none());
    assert_eq!(active_email(&mut app).as_deref(), Some("lin@example.com"));
    assert!(app.shown(&row_selector("lin@example.com", true)));
}

#[gpui::test]
fn removing_an_account_asks_first_and_the_active_one_cannot_go(cx: &mut TestAppContext) {
    let scenario = MockScenario {
        accounts: vec![
            MockAccount {
                email: "ada@example.com".into(),
                balance_cu: 100.0,
            },
            MockAccount {
                email: "lin@example.com".into(),
                balance_cu: 50.0,
            },
        ],
        runtimes: vec![],
    };
    let mut app = TestApp::with(cx, Options::scenario(scenario));
    on_accounts(&mut app);
    let blocks: Vec<Option<RemoveBlock>> = app.read(|state, _| {
        state
            .account_rows()
            .iter()
            .map(|row| row.remove_block)
            .collect()
    });
    assert_eq!(blocks, [Some(RemoveBlock::Active), None]);
    app.click(&remove_selector("ada@example.com"));
    assert!(
        dialog(&mut app).is_none(),
        "Remove is disabled on the active account"
    );

    app.click(&remove_selector("lin@example.com"));
    assert!(matches!(
        dialog(&mut app),
        Some(AccountDialog::Remove { .. })
    ));
    app.click("remove-confirm");
    assert!(dialog(&mut app).is_none());
    assert_eq!(emails(&mut app), ["ada@example.com"]);
}

#[gpui::test]
fn an_account_with_runtimes_cannot_be_removed_yet(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    let lin = app.read(|state, _| {
        state
            .account_rows()
            .into_iter()
            .find(|row| row.account.email.as_deref() == Some("lin@example.com"))
            .map(|row| row.remove_block)
    });
    assert_eq!(lin, Some(Some(RemoveBlock::Running)));
}

#[gpui::test]
fn an_unreachable_keyring_is_explained_and_checked_again(cx: &mut TestAppContext) {
    let options = Options {
        failures: MockFailures {
            keyring_unavailable: true,
            ..MockFailures::default()
        },
        ..Options::default()
    };
    let mut app = TestApp::with(cx, options);
    assert!(matches!(
        app.read(|state, _| state.sign_in_status()),
        SignInStatus::KeyringUnavailable(_)
    ));
    on_accounts(&mut app);
    assert!(app.shown("accounts-keyring"));

    app.set_failures(MockFailures::default());
    app.click("keyring-again");
    assert_eq!(active_email(&mut app).as_deref(), Some("ada@example.com"));
}

#[test]
fn a_signed_in_but_unsaved_cli_account_is_offered_for_saving() {
    let probe = Load::Ready(ProbeReport {
        colab: Some("/usr/bin/colab".into()),
        ..ProbeReport::default()
    });
    let view = AccountsView {
        accounts: vec![],
        active: None,
        has_token: true,
        unadopted: true,
        active_email: Some("eve@example.com".into()),
        recovered_pending_add: false,
    };
    assert_eq!(
        sign_in_status(&probe, &Load::Ready(view.clone())),
        SignInStatus::Unadopted {
            email: Some("eve@example.com".into())
        }
    );
    let signed_out = AccountsView {
        has_token: false,
        unadopted: false,
        active_email: None,
        ..view
    };
    assert_eq!(
        sign_in_status(&probe, &Load::Ready(signed_out)),
        SignInStatus::SignedOut
    );
    assert_eq!(
        sign_in_status(&Load::Ready(ProbeReport::default()), &Load::Idle),
        SignInStatus::NeedsCli
    );
}
