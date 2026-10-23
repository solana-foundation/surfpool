use ratatui::backend::TestBackend;

use super::*;

#[test]
fn a_historical_surfnet_names_what_it_forks_after_instead_of_offline_mode() {
    let (_, events) = unbounded();
    let (commands, _) = unbounded();
    let config = SanitizedConfig {
        historical: Some(surfpool_types::SanitizedHistoricalConfig {
            after: "413023260".to_string(),
        }),
        ..SanitizedConfig::default()
    };
    let app = App::new(
        events,
        commands,
        false,
        vec![],
        DisplayedUrl::Historical(config),
        None,
        0,
    );
    let messages: Vec<_> = app
        .events
        .iter()
        .map(|(_, _, message)| message.clone())
        .collect();
    assert!(
        messages.iter().any(|m| m.contains("after 413023260")),
        "{messages:?}"
    );
    assert!(
        !messages.iter().any(|m| m.contains("offline mode")),
        "{messages:?}"
    );
}

fn paused_app(historical: bool) -> App {
    let (_, events) = unbounded();
    let (commands, _) = unbounded();
    let config = SanitizedConfig::default();
    let displayed = if historical {
        DisplayedUrl::Historical(config)
    } else {
        DisplayedUrl::Datasource(config)
    };
    let mut app = App::new(events, commands, false, vec![], displayed, None, 0);
    app.clock.slot = 413023261;
    app.historical_loading_since = None;
    app.paused = true;
    app
}

fn slot_bar(app: &mut App) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(100, 1)).unwrap();
    terminal
        .draw(|frame| render_slot_bar(frame, app, frame.area()))
        .unwrap();
    terminal.backend().buffer().clone()
}

#[test]
fn frozen_historical_slot_bar_does_not_blink() {
    let mut app = paused_app(true);
    let initial = slot_bar(&mut app);
    for _ in 0..4 {
        app.last_blink = Instant::now() - Duration::from_millis(600);
        app.update_blink_state();
        assert_eq!(slot_bar(&mut app), initial);
    }
}

#[test]
fn normal_paused_surfpool_keeps_its_animation() {
    let mut app = paused_app(false);
    let initial = slot_bar(&mut app);
    app.last_blink = Instant::now() - Duration::from_millis(600);
    app.update_blink_state();
    assert_ne!(slot_bar(&mut app), initial);
}

#[test]
fn explicit_historical_slot_step_updates_the_static_bar() {
    let mut app = paused_app(true);
    let initial = slot_bar(&mut app);
    app.clock.slot += 1;
    assert_ne!(slot_bar(&mut app), initial);
}

fn header(app: &mut App, width: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, 3)).unwrap();
    terminal
        .draw(|frame| render_header(frame, app, frame.area()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn historical_startup_shows_loading_instead_of_zero_slot_and_epoch() {
    for width in [100, 190] {
        let mut app = paused_app(true);
        app.historical_loading_since = Some(Instant::now());
        let rendered = header(&mut app, width);
        assert!(rendered.contains("Loading historical state"), "{rendered}");
        assert!(rendered.contains("SLOT —"), "{rendered}");
        assert!(rendered.contains("EPOCH  —"), "{rendered}");
        assert!(!rendered.contains("SLOT 0"));
        assert!(!rendered.contains("EPOCH  0"));
        assert!(!rendered.contains("413023261"));
        assert!(
            !app.events
                .iter()
                .any(|(_, _, text)| text.contains("up and running"))
        );
    }
}

#[test]
fn ordinary_surfpool_does_not_show_historical_loading() {
    let mut app = paused_app(false);
    let rendered = header(&mut app, 190);
    assert!(!rendered.contains("Loading historical state"));
    assert!(rendered.contains("413023261"));
    assert!(
        app.events
            .iter()
            .any(|(_, _, text)| text.contains("up and running"))
    );
}

#[test]
fn loading_timer_updates_then_disappears_when_historical_clock_is_ready() {
    let (_, events) = unbounded();
    let (commands, _) = unbounded();
    let mut app = App::new(
        events,
        commands,
        false,
        vec![],
        DisplayedUrl::Historical(SanitizedConfig::default()),
        None,
        0,
    );
    assert!(app.historical_loading_since.is_some());
    app.historical_loading_since = Some(Instant::now() - Duration::from_secs(7));
    assert!(header(&mut app, 190).contains("7s elapsed"));
    app.clock.slot = 413023261;
    app.epoch_info.epoch = 956;
    app.historical_loading_since = None;
    let rendered = header(&mut app, 190);
    assert!(!rendered.contains("Loading historical state"));
    assert!(rendered.contains("SLOT 413023261"));
    assert!(rendered.contains("EPOCH  956"));
}
