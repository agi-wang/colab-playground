//! The sidebar: W-ID status, navigation, and the usage and demo badges. The app mark
//! sits above it in the title strip (`titlebar`).

use colab_playground_core::backend::AccountUsageView;
use ely_gpui_component::{
    data_display::{Badge, Tone},
    layout::Sidebar,
    navigation::NavItem,
    primitives::{IconName, Tooltip},
};
use gpui::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div,
};

use crate::{
    backend, format,
    i18n::{keys, tr, tr_args},
    route::Route,
    state::{AppState, Load, usage_is_stale, usage_stale_note},
    ui::{tagged, usage::eta},
};

pub const ACCOUNT_SLOT_SELECTOR: &str = "sidebar-account";
/// Selector of the balance and burn-rate badge.
pub const USAGE_SLOT_SELECTOR: &str = "sidebar-usage";
/// Selector of the demo-data badge shown with the mock backend.
pub const DEMO_BADGE_SELECTOR: &str = "sidebar-demo";
/// Selector around the Settings entry while a newer release is available.
pub const UPDATE_MARK_SELECTOR: &str = "sidebar-update";

/// The sidebar for `state`; `handle` is the same state, for click handlers.
pub fn render(state: &AppState, handle: &Entity<AppState>, cx: &App) -> impl IntoElement {
    let current = state.route();
    // A newer release stays marked on Settings (where About offers it) until this build is it.
    let update = state.update_available().is_some();
    let items = Route::ALL.map(|route| {
        let handle = handle.clone();
        let item = NavItem::new(
            route.nav_selector(),
            route.icon(),
            tr(cx, route.title_key()),
        )
        .active(route == current)
        .on_click(move |_, cx| handle.update(cx, |state, cx| state.navigate(route, cx)));
        let item = if update && route == Route::Settings {
            item.count(tr(cx, keys::UPDATE_BADGE))
        } else {
            item
        };
        let marked = if update && route == Route::Settings {
            UPDATE_MARK_SELECTOR
        } else {
            ""
        };
        tagged(route.nav_selector(), tagged(marked, item))
    });
    Sidebar::new("sidebar", false).child(
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_2()
            .child(account_slot(state, handle, cx))
            .child(div().flex().flex_col().gap_0p5().children(items))
            .child(div().flex_1())
            .child(badges(state, cx)),
    )
}

/// The W-ID identity. Google account switching lives on the Accounts page.
fn account_slot(state: &AppState, handle: &Entity<AppState>, cx: &App) -> impl IntoElement {
    let (status, tip) = if state.is_busy(crate::state::Op::SyncStatus) {
        (
            tr(cx, keys::W_ID_SIDEBAR_CHECKING),
            tr(cx, keys::SYNC_STATUS_CHECKING),
        )
    } else {
        match state.sync_status() {
            Load::Ready(signed_in) if signed_in.signed_in => (
                tr(cx, keys::W_ID_SIDEBAR_SIGNED_IN),
                signed_in
                    .email
                    .as_deref()
                    .map_or_else(|| tr(cx, keys::SYNC_STATUS_SIGNED_IN), SharedString::from),
            ),
            Load::Ready(_) => (
                tr(cx, keys::W_ID_SIDEBAR_SIGNED_OUT),
                tr(cx, keys::SYNC_STATUS_SIGNED_OUT),
            ),
            Load::Failed(_) => (
                tr(cx, keys::W_ID_SIDEBAR_FAILED),
                tr(cx, keys::SYNC_STATUS_FAILED),
            ),
            Load::Idle | Load::Loading => (
                tr(cx, keys::W_ID_SIDEBAR_CHECKING),
                tr(cx, keys::SYNC_STATUS_CHECKING),
            ),
        }
    };
    let handle = handle.clone();
    div()
        .id(ACCOUNT_SLOT_SELECTOR)
        .tooltip(Tooltip::text(tip))
        .child(tagged(
            ACCOUNT_SLOT_SELECTOR,
            NavItem::new("account-slot", IconName::User, "W-ID")
                .count(status)
                .on_click(move |_, cx| handle.update(cx, |state, cx| state.open_sync(cx))),
        ))
}

/// Balance · burn rate of the active account (stale marked, unknown never shown as zero) and
/// the demo-data badge.
fn badges(state: &AppState, cx: &App) -> impl IntoElement {
    let usage = state.active_usage();
    let (text, tone) = match usage.and_then(|usage| usage.snapshot) {
        Some(snapshot) => {
            let text = tr_args(
                cx,
                keys::USAGE_BADGE,
                &[
                    ("balance", &format::cu(cx, snapshot.balance_cu)),
                    ("rate", &format::cu_rate(cx, snapshot.rate_cu_per_hour)),
                ],
            );
            if usage_is_stale(&snapshot, state.usage_now()) {
                (
                    SharedString::from(format!("{text} · {}", tr(cx, keys::USAGE_OUTDATED))),
                    Tone::Warning,
                )
            } else {
                (text, Tone::Neutral)
            }
        }
        None => (tr(cx, keys::SHELL_USAGE), Tone::Neutral),
    };
    let tip = usage_tip(usage, state, cx);
    div()
        .flex()
        .flex_wrap()
        .gap_2()
        .px_2()
        .child(
            div()
                .id(USAGE_SLOT_SELECTOR)
                .tooltip(Tooltip::text(tip))
                .child(tagged(
                    format!("{USAGE_SLOT_SELECTOR} {text}"),
                    tagged(USAGE_SLOT_SELECTOR, Badge::new(text).tone(tone)),
                )),
        )
        .children(backend::shows_demo_badge(state.flavor()).then(|| {
            div()
                .id(DEMO_BADGE_SELECTOR)
                .tooltip(Tooltip::text(tr(cx, keys::SHELL_BACKEND_MOCK_HINT)))
                .child(tagged(
                    DEMO_BADGE_SELECTOR,
                    Badge::new(tr(cx, keys::SHELL_BACKEND_MOCK)).tone(Tone::Warning),
                ))
        }))
}

/// The badge's tooltip: time left, when it was updated, and whether the values are stale.
pub fn usage_tip(usage: Option<&AccountUsageView>, state: &AppState, cx: &App) -> SharedString {
    let Some(snapshot) = usage.and_then(|usage| usage.snapshot) else {
        return tr(cx, keys::USAGE_TIP_UNKNOWN);
    };
    let mut lines = vec![tr(cx, keys::SHELL_USAGE_HINT)];
    lines.push(match eta(usage, &snapshot) {
        None => tr(cx, keys::USAGE_TIP_IDLE),
        Some(eta) if eta.is_zero() => tr(cx, keys::USAGE_TIP_EMPTY),
        Some(eta) => tr_args(
            cx,
            keys::USAGE_TIP_LEFT,
            &[("time", &format::duration(cx, eta))],
        ),
    });
    if let Some(now) = state.usage_now() {
        lines.push(tr_args(
            cx,
            keys::USAGE_TIP_UPDATED,
            &[("age", &format::age(cx, snapshot.fetched_at, now))],
        ));
    }
    if usage_is_stale(&snapshot, state.usage_now()) {
        lines.push(tr(cx, usage_stale_note(&snapshot)));
    }
    let joined: Vec<&str> = lines.iter().map(SharedString::as_ref).collect();
    joined.join("\n").into()
}
