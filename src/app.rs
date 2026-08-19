// SPDX-License-Identifier: GPL-3.0

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use cosmic::app::{Core, Task};
use cosmic::cctk::sctk::reexports::protocols::xdg::shell::client::xdg_positioner::{
    Anchor, Gravity,
};
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::widget::text::LineHeight;
use cosmic::iced::window::Id;
use cosmic::iced::{Alignment, Length, Limits, Subscription};
use cosmic::surface::action::{app_popup, destroy_popup};
use cosmic::widget;
use cosmic::Element;
use mpris::PlaybackStatus;

use crate::config::Config;
use crate::mpris::{PlayerInfo, PlayerSummary, Poll};

static PANEL_AUTOSIZE_ID: LazyLock<widget::Id> =
    LazyLock::new(|| widget::Id::new("on-blast-panel"));

/// Minimum panel height for the two-line title/artist stack; thinner panels
/// fall back to a single line.
const MIN_STACK_PANEL_HEIGHT: u16 = 28;

/// Relative line height for stacked panel text; >1.2 keeps descenders inside
/// the line box.
const STACK_LINE_HEIGHT: f32 = 1.3;

/// Glyph prefixes for the panel's track and artist lines.
const TRACK_ICON: &str = "emblem-music-symbolic";
const ARTIST_ICON: &str = "system-users-symbolic";

/// Shown in the panel when nothing is playing.
const IDLE_ICON: &str = "io.github.cosmic-ext-applet-on-blast-symbolic";

/// Stands in for a missing cover so the panel width stays stable.
const NO_ART_ICON: &str = "media-optical-symbolic";

/// Art fetches per URL before giving up. At the 500ms poll cadence that retries
/// for ~3s, enough to ride out a server generating a cover variant on demand.
const MAX_ART_ATTEMPTS: u8 = 6;

#[derive(Default)]
pub struct AppModel {
    core: Core,
    popup: Option<Id>,
    settings_popup: Option<Id>,
    /// Main popup size, used to anchor the settings popup beside the gear.
    popup_size: Option<(f32, f32)>,
    config: Config,
    player: PlayerInfo,
    album_art: Option<cosmic::iced::widget::image::Handle>,
    /// Blurred copy of the cover, used as the popup's full-bleed backdrop.
    album_art_blurred: Option<cosmic::iced::widget::image::Handle>,
    /// Average cover colour (rgba 0..1), painted behind the blurred backdrop so
    /// the frame is never bare while art loads.
    album_art_color: Option<[f32; 4]>,
    current_art_url: Option<String>,
    /// Fetch attempts for `current_art_url`; reset when the URL changes.
    art_attempts: u8,
    seeking: Option<f64>,
    missed_polls: u8,
    /// All MPRIS players seen on the last poll, for the picker.
    players: Vec<PlayerSummary>,
    /// User-pinned player (trimmed bus name); `None` means auto-pick.
    selected_player: Option<String>,
    /// True while a poll runs, so overlapping ticks don't stack blocking D-Bus
    /// work (and extra worker threads/connections).
    poll_in_flight: bool,
    /// Base ticks elapsed since the last poll, for adaptive cadence.
    ticks_since_poll: u8,
    /// Accumulated scroll notches, so a touchpad's pixel stream skips one track
    /// per notch rather than per event.
    scroll_accum: f32,
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    ToggleSettings,
    PopupClosed(Id),
    UpdateConfig(Config),
    Tick,
    Polled(Poll),
    SelectPlayer(String),
    ArtLoaded {
        url: String,
        handle: Option<cosmic::iced::widget::image::Handle>,
        blurred: Option<cosmic::iced::widget::image::Handle>,
        color: Option<[f32; 4]>,
    },
    PlayPause,
    Next,
    Previous,
    Scroll(f32),
    LabelMaxLengthChanged(u32),
    TrackFirstChanged(bool),
    SeekChanged(f64),
    SeekCommit,
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = "io.github.cosmic-ext-applet-on-blast";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        let config = cosmic_config::Config::new(Self::APP_ID, Config::VERSION)
            .map(|ctx| match Config::get_entry(&ctx) {
                Ok(cfg) => cfg,
                Err((_, cfg)) => cfg,
            })
            .unwrap_or_default();

        let app = AppModel { core, config, ..Default::default() };
        (app, Task::none())
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn on_window_resize(&mut self, id: Id, width: f32, height: f32) {
        if self.popup == Some(id) {
            self.popup_size = Some((width, height));
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let max_len = self.config.panel_label_max_length as usize;
        let panel_size = self.core.applet.suggested_size(false);
        let (_, vert_pad) = self.core.applet.suggested_padding(false);
        let thumb_size = panel_size.1;
        let height = (thumb_size + 2 * vert_pad) as f32;

        let button_content: Element<'_, Message> = if !self.player.bus_name.is_empty() {
            let mut children: Vec<Element<'_, Message>> = Vec::new();

            if let Some(ref handle) = self.album_art {
                children.push(
                    widget::image(handle.clone())
                        .width(Length::Fixed(thumb_size as f32))
                        .height(Length::Fixed(thumb_size as f32))
                        .content_fit(cosmic::iced::ContentFit::Cover)
                        .into(),
                );
            } else {
                children.push(
                    widget::icon::from_name(NO_ART_ICON).size(thumb_size).into(),
                );
            }

            let text_el: Element<'_, Message> = if !self.player.artist.is_empty()
                && thumb_size >= MIN_STACK_PANEL_HEIGHT
            {
                // Two lines at a smaller font, each prefixed by a glyph.
                let (top, top_icon, bottom, bottom_icon) = if self.config.track_first
                {
                    (&self.player.title, TRACK_ICON, &self.player.artist, ARTIST_ICON)
                } else {
                    (&self.player.artist, ARTIST_ICON, &self.player.title, TRACK_ICON)
                };
                // Sized so two line boxes fit the panel, with a little room around.
                let line_size =
                    ((height * 0.9) / (2.0 * STACK_LINE_HEIGHT)).clamp(8.0, 13.0);
                let icon_size = line_size.round() as u16;
                let line = |icon: &str, label: &str| -> Element<'_, Message> {
                    widget::row(vec![
                        widget::icon::from_name(icon).size(icon_size).into(),
                        widget::text(truncate_label(label, max_len))
                            .size(line_size)
                            .line_height(LineHeight::Relative(STACK_LINE_HEIGHT))
                            .into(),
                    ])
                    .spacing(4)
                    .align_y(Alignment::Center)
                    .into()
                };
                widget::column(vec![
                    line(top_icon, top),
                    line(bottom_icon, bottom),
                ])
                .into()
            } else if self.player.artist.is_empty() {
                // Inline glyphs sized to the body text, not the full panel height.
                let glyph = ((thumb_size as f32) * 0.6).round().clamp(12.0, 18.0) as u16;
                widget::row(vec![
                    widget::icon::from_name(TRACK_ICON).size(glyph).into(),
                    self.core
                        .applet
                        .text(truncate_label(&self.player.title, max_len))
                        .into(),
                ])
                .spacing(4)
                .align_y(Alignment::Center)
                .into()
            } else {
                // Single line: track and artist separated by a dash.
                let glyph = ((thumb_size as f32) * 0.6).round().clamp(12.0, 18.0) as u16;
                let (first, first_icon, second, second_icon) =
                    if self.config.track_first {
                        (&self.player.title, TRACK_ICON, &self.player.artist, ARTIST_ICON)
                    } else {
                        (&self.player.artist, ARTIST_ICON, &self.player.title, TRACK_ICON)
                    };
                widget::row(vec![
                    widget::icon::from_name(first_icon).size(glyph).into(),
                    self.core.applet.text(truncate_label(first, max_len)).into(),
                    self.core.applet.text("\u{2014}").into(),
                    widget::icon::from_name(second_icon).size(glyph).into(),
                    self.core.applet.text(truncate_label(second, max_len)).into(),
                ])
                .spacing(4)
                .align_y(Alignment::Center)
                .into()
            };
            children.push(text_el);

            widget::row(children)
                .spacing(10)
                .align_y(Alignment::Center)
                .into()
        } else {
            widget::icon::from_name(IDLE_ICON).size(panel_size.0).into()
        };

        use cosmic::iced::mouse;
        widget::autosize::autosize(
            widget::mouse_area(
                widget::button::custom(
                    widget::container(button_content)
                        .center_y(Length::Fixed(height)),
                )
                .height(Length::Fixed(height))
                .class(cosmic::theme::Button::AppletIcon)
                .on_press_down(Message::TogglePopup),
            )
            .on_scroll(|delta| {
                // A wheel line is one notch; touchpad pixels scale down so a
                // fling doesn't skip a dozen tracks.
                let notches = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / 50.0,
                };
                Message::Scroll(notches)
            })
            .on_middle_press(Message::PlayPause),
            PANEL_AUTOSIZE_ID.clone(),
        )
        .into()
    }

    fn view_window(&self, _id: Id) -> Element<'_, Message> {
        // Popup is rendered via the app_popup closure in update(); this is a stub.
        widget::text("").into()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            cosmic::iced::time::every(Duration::from_millis(500)).map(|_| Message::Tick),
            self.core()
                .watch_config::<Config>(Self::APP_ID)
                .map(|update| Message::UpdateConfig(update.config)),
        ])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::TogglePopup => {
                if let Some(id) = self.popup.take() {
                    // Tear down the settings popup too, so it can't outlive the
                    // main popup it's anchored to.
                    let mut tasks = vec![cosmic::task::message(cosmic::Action::Cosmic(
                        cosmic::app::Action::Surface(destroy_popup(id)),
                    ))];
                    if let Some(sid) = self.settings_popup.take() {
                        tasks.push(cosmic::task::message(cosmic::Action::Cosmic(
                            cosmic::app::Action::Surface(destroy_popup(sid)),
                        )));
                    }
                    return Task::batch(tasks);
                } else {
                    let surface = cosmic::task::message(cosmic::Action::Cosmic(
                        cosmic::app::Action::Surface(app_popup::<AppModel>(
                            |state: &mut AppModel| {
                                let new_id = Id::unique();
                                state.popup = Some(new_id);
                                state.core.applet.get_popup_settings(
                                    state.core.main_window_id().unwrap(),
                                    new_id,
                                    None,
                                    None,
                                    None,
                                )
                            },
                            Some(Box::new(|state: &AppModel| {
                                let cosmic_theme = cosmic::theme::active();
                                let spacing = cosmic_theme.cosmic().spacing;
                                let space_s: f32 = spacing.space_s.into();
                                let space_m: f32 = spacing.space_m.into();
                                // Every layer of the art stack rounds to the frame's
                                // radius, so the popup follows the theme.
                                let art_radius = cosmic_theme.cosmic().corner_radii.radius_m;
                                let hero_size = (POPUP_WIDTH - 2.0 * space_m).max(0.0);

                                let status_icon = match &state.player.status {
                                    PlaybackStatus::Playing => "media-playback-pause-symbolic",
                                    _ => "media-playback-start-symbolic",
                                };

                                let seek_pos = state
                                    .seeking
                                    .unwrap_or(state.player.position_us as f64);

                                // Always a bar plus a time row, so the block height
                                // never changes with the track.
                                let has_length = state.player.length_us > 0;
                                let frac = if has_length {
                                    (seek_pos / state.player.length_us as f64).clamp(0.0, 1.0)
                                        as f32
                                } else {
                                    0.0
                                };
                                let time_row = widget::row(vec![
                                    widget::text::caption(format_time(seek_pos as u64)).into(),
                                    widget::Space::new().width(Length::Fill).into(),
                                    widget::text::caption(format_time(state.player.length_us))
                                        .into(),
                                ]);
                                // Read-only bar when the player can't seek.
                                let bar: Element<'_, Message> =
                                    if has_length && state.player.can_seek {
                                        widget::slider(
                                            0.0..=state.player.length_us as f64,
                                            seek_pos,
                                            Message::SeekChanged,
                                        )
                                        .on_release(Message::SeekCommit)
                                        .width(Length::Fill)
                                        .into()
                                    } else {
                                        widget::container(widget::determinate_linear(frac))
                                            .width(Length::Fill)
                                            .into()
                                    };
                                let progress: Element<'_, Message> =
                                    widget::column(vec![bar, time_row.into()])
                                        .spacing(2.0)
                                        .into();

                                // Each button is pressable only when the player
                                // advertises the matching capability.
                                let mut control_row: Vec<Element<'_, Message>> = Vec::new();

                                let mut prev = widget::button::icon(widget::icon::from_name(
                                    "media-skip-backward-symbolic",
                                ))
                                .class(scrim_button());
                                if state.player.can_go_previous {
                                    prev = prev.on_press(Message::Previous);
                                }
                                control_row.push(prev.into());

                                let can_playpause = match state.player.status {
                                    PlaybackStatus::Playing => state.player.can_pause,
                                    _ => state.player.can_play,
                                };
                                // Primary action, so a size up from the skips.
                                let mut play_pause =
                                    widget::button::icon(widget::icon::from_name(status_icon))
                                        .medium()
                                        .class(scrim_button());
                                if can_playpause {
                                    play_pause = play_pause.on_press(Message::PlayPause);
                                }
                                control_row.push(play_pause.into());

                                let mut next = widget::button::icon(widget::icon::from_name(
                                    "media-skip-forward-symbolic",
                                ))
                                .class(scrim_button());
                                if state.player.can_go_next {
                                    next = next.on_press(Message::Next);
                                }
                                control_row.push(next.into());

                                // Fill spacers keep the transport centred with the
                                // gear tucked into the corner.
                                let gear = widget::button::icon(widget::icon::from_name(
                                    "emblem-system-symbolic",
                                ))
                                .class(scrim_button())
                                .on_press(Message::ToggleSettings);

                                let controls: Element<'_, Message> = widget::row(vec![
                                    widget::Space::new().width(Length::Fill).into(),
                                    widget::row(control_row)
                                        .spacing(space_s)
                                        .align_y(Alignment::Center)
                                        .into(),
                                    widget::container(gear)
                                        .width(Length::Fill)
                                        .align_x(cosmic::iced::alignment::Horizontal::Right)
                                        .into(),
                                ])
                                .align_y(Alignment::Center)
                                .into();

                                // A blurred copy of the cover replaces the theme
                                // surface; `round_corners` masks its corners.
                                let backdrop: Element<'_, Message> = {
                                    let color = state
                                        .album_art_color
                                        .unwrap_or([0.0, 0.0, 0.0, 1.0]);
                                    let bg = cosmic::iced::Color::from_rgba(
                                        color[0], color[1], color[2], color[3],
                                    );
                                    let inner: Element<'_, Message> =
                                        if let Some(ref handle) = state.album_art_blurred {
                                            widget::image(handle.clone())
                                                .width(Length::Fill)
                                                .height(Length::Fill)
                                                .content_fit(cosmic::iced::ContentFit::Fill)
                                                .into()
                                        } else {
                                            widget::Space::new().into()
                                        };
                                    widget::container(inner)
                                        .width(Length::Fill)
                                        .height(Length::Fill)
                                        .class(cosmic::theme::Container::custom(move |_theme| {
                                            cosmic::iced::widget::container::Style {
                                                background: Some(
                                                    cosmic::iced::Background::Color(bg),
                                                ),
                                                border: cosmic::iced::Border {
                                                    radius: art_radius.into(),
                                                    ..Default::default()
                                                },
                                                ..Default::default()
                                            }
                                        }))
                                        .into()
                                };

                                let hero: Element<'_, Message> =
                                    if let Some(ref handle) = state.album_art {
                                        widget::image(handle.clone())
                                            .width(Length::Fixed(hero_size))
                                            .height(Length::Fixed(hero_size))
                                            .content_fit(cosmic::iced::ContentFit::Cover)
                                            .into()
                                    } else {
                                        widget::container(
                                            widget::icon::from_name(
                                                "audio-headphones-symbolic",
                                            )
                                            .size(64),
                                        )
                                        .width(Length::Fixed(hero_size))
                                        .height(Length::Fixed(hero_size))
                                        .align_x(cosmic::iced::alignment::Horizontal::Center)
                                        .align_y(cosmic::iced::alignment::Vertical::Center)
                                        .class(cosmic::theme::Container::Card)
                                        .into()
                                    };

                                // One non-wrapping line each, so the block is the
                                // same height for every track.
                                use cosmic::iced::advanced::text::Wrapping;
                                let mut info: Vec<Element<'_, Message>> = vec![
                                    widget::text::title4(truncate_label(&state.player.title, 34))
                                        .wrapping(Wrapping::None)
                                        .into(),
                                ];
                                let mut secondary = state.player.artist.clone();
                                if !state.player.album.is_empty() {
                                    let album_str = if let Some(y) = state.player.year {
                                        format!("{} ({})", state.player.album, y)
                                    } else {
                                        state.player.album.clone()
                                    };
                                    secondary = if secondary.is_empty() {
                                        album_str
                                    } else {
                                        format!("{secondary} \u{00b7} {album_str}")
                                    };
                                }
                                // A space when empty, so the line never collapses.
                                let secondary = if secondary.is_empty() {
                                    " ".to_string()
                                } else {
                                    truncate_label(&secondary, 44)
                                };
                                info.push(
                                    widget::text::body(secondary)
                                        .wrapping(Wrapping::None)
                                        .into(),
                                );

                                // One row per player, only when there's a choice.
                                let player_picker: Option<Element<'_, Message>> =
                                    if state.players.len() > 1 {
                                        let rows: Vec<Element<'_, Message>> = state
                                            .players
                                            .iter()
                                            .map(|s| {
                                                let is_current =
                                                    s.bus_name == state.player.bus_name;
                                                let status_icon = match s.status {
                                                    PlaybackStatus::Playing => {
                                                        "media-playback-start-symbolic"
                                                    }
                                                    PlaybackStatus::Paused => {
                                                        "media-playback-pause-symbolic"
                                                    }
                                                    _ => "media-playback-stop-symbolic",
                                                };
                                                let label = if s.title.is_empty() {
                                                    s.identity.clone()
                                                } else {
                                                    format!("{} \u{2014} {}", s.identity, s.title)
                                                };
                                                widget::button::custom(
                                                    widget::row(vec![
                                                        widget::icon::from_name(status_icon)
                                                            .size(14)
                                                            .into(),
                                                        widget::text::body(truncate_label(
                                                            &label, 42,
                                                        ))
                                                        .into(),
                                                    ])
                                                    .spacing(space_s)
                                                    .align_y(Alignment::Center),
                                                )
                                                .class(scrim_row_button(is_current))
                                                .width(Length::Fill)
                                                .on_press(Message::SelectPlayer(
                                                    s.bus_name.clone(),
                                                ))
                                                .into()
                                            })
                                            .collect();
                                        Some(widget::column(rows).spacing(2.0).into())
                                    } else {
                                        None
                                    };

                                // Everything shares one scrim over the blur, so no
                                // control depends on how bright the cover is.
                                let mut body_children: Vec<Element<'_, Message>> = Vec::new();
                                if let Some(picker) = player_picker {
                                    body_children.push(picker);
                                }
                                body_children.extend([
                                    widget::container(hero)
                                        .width(Length::Fill)
                                        .align_x(cosmic::iced::alignment::Horizontal::Center)
                                        .into(),
                                    widget::column(info).spacing(2.0).into(),
                                    progress,
                                    controls,
                                ]);
                                let foreground_body: Element<'_, Message> =
                                    widget::column(body_children).spacing(space_s).into();

                                let foreground = widget::container(foreground_body)
                                    .width(Length::Fill)
                                    .padding(space_m)
                                    .class(cosmic::theme::Container::Custom(Box::new(
                                        move |_theme| cosmic::iced::widget::container::Style {
                                            background: Some(cosmic::iced::Background::Color(
                                                cosmic::iced::Color::from_rgba(
                                                    0.0, 0.0, 0.0, 0.4,
                                                ),
                                            )),
                                            text_color: Some(cosmic::iced::Color::WHITE),
                                            border: cosmic::iced::Border {
                                                radius: art_radius.into(),
                                                ..Default::default()
                                            },
                                            snap: true,
                                            ..Default::default()
                                        },
                                    )));

                                // The scrim is the base layer so the stack sizes to
                                // content; the backdrop fills exactly that box.
                                let art: Element<'_, Message> =
                                    cosmic::iced::widget::Stack::new()
                                        .push(foreground)
                                        .push_under(backdrop)
                                        .width(Length::Fill)
                                        .clip(true)
                                        .into();

                                Element::from(
                                    state
                                        .core
                                        .applet
                                        .popup_container(art)
                                        .limits(
                                            Limits::NONE
                                                .min_width(POPUP_WIDTH)
                                                .max_width(POPUP_WIDTH)
                                                .min_height(200.0)
                                                .max_height(750.0),
                                        ),
                                )
                                .map(cosmic::Action::App)
                            })),
                        )),
                    ));
                    // Kick an immediate poll so the popup opens with position and
                    // controls populated rather than waiting for the next tick.
                    if self.poll_in_flight {
                        return surface;
                    }
                    return Task::batch([surface, self.spawn_poll(true)]);
                }
            }

            Message::ToggleSettings => {
                if let Some(id) = self.settings_popup.take() {
                    return cosmic::task::message(cosmic::Action::Cosmic(
                        cosmic::app::Action::Surface(destroy_popup(id)),
                    ));
                } else {
                    return cosmic::task::message(cosmic::Action::Cosmic(
                        cosmic::app::Action::Surface(app_popup::<AppModel>(
                            |state: &mut AppModel| {
                                let new_id = Id::unique();
                                state.settings_popup = Some(new_id);
                                // Anchor to the main popup while it's open; fall
                                // back to the panel window otherwise.
                                let parent = state
                                    .popup
                                    .unwrap_or_else(|| state.core.main_window_id().unwrap());
                                let mut settings = state.core.applet.get_popup_settings(
                                    parent,
                                    new_id,
                                    None,
                                    None,
                                    None,
                                );
                                let space_m: f32 = cosmic::theme::active()
                                    .cosmic()
                                    .spacing
                                    .space_m
                                    .into();
                                // Grow up-and-right from the parent's bottom-right,
                                // so the two popups share a bottom edge.
                                settings.positioner.anchor = Anchor::BottomRight;
                                settings.positioner.gravity = Gravity::TopRight;
                                settings.positioner.offset = (space_m as i32, 0);
                                if let Some((w, h)) = state.popup_size {
                                    let gear = 32.0_f32;
                                    settings.positioner.anchor_rect =
                                        cosmic::iced::Rectangle {
                                            x: (w - gear).max(0.0) as i32,
                                            y: (h - gear).max(0.0) as i32,
                                            width: gear as i32,
                                            height: gear as i32,
                                        };
                                }
                                settings
                            },
                            Some(Box::new(|state: &AppModel| {
                                let spacing = cosmic::theme::active().cosmic().spacing;
                                let space_m: f32 = spacing.space_m.into();
                                let space_s: f32 = spacing.space_s.into();

                                let content = widget::column(vec![
                                    widget::text::title4("Settings").into(),
                                    widget::settings::item(
                                        "Panel label length",
                                        cosmic::widget::spin_button(
                                            state.config.panel_label_max_length.to_string(),
                                            state.config.panel_label_max_length,
                                            1u32,
                                            10u32,
                                            100u32,
                                            Message::LabelMaxLengthChanged,
                                        ),
                                    )
                                    .into(),
                                    widget::settings::item(
                                        "Track before artist",
                                        widget::toggler(state.config.track_first)
                                            .on_toggle(Message::TrackFirstChanged),
                                    )
                                    .into(),
                                ])
                                .spacing(space_s)
                                .padding(space_m);

                                Element::from(
                                    state
                                        .core
                                        .applet
                                        .popup_container(content)
                                        .limits(
                                            Limits::NONE
                                                .min_width(260.0)
                                                .max_width(360.0)
                                                .min_height(100.0)
                                                .max_height(500.0),
                                        ),
                                )
                                .map(cosmic::Action::App)
                            })),
                        )),
                    ));
                }
            }

            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                }
                if self.settings_popup.as_ref() == Some(&id) {
                    self.settings_popup = None;
                }
            }

            Message::UpdateConfig(config) => {
                self.config = config;
            }

            Message::Tick => {
                self.ticks_since_poll = self.ticks_since_poll.saturating_add(1);
                // Position is the only sub-second field and it's popup-only, so
                // poll fast only while the popup is open.
                let popup_open = self.popup.is_some();
                let desired = if popup_open {
                    1
                } else if self.player.status == PlaybackStatus::Playing {
                    2
                } else {
                    4
                };
                if self.poll_in_flight || self.ticks_since_poll < desired {
                    return Task::none();
                }
                return self.spawn_poll(popup_open);
            }

            Message::SelectPlayer(bus_name) => {
                // Reset per-player state so the old art doesn't linger.
                self.selected_player = Some(bus_name);
                self.album_art = None;
                self.album_art_blurred = None;
                self.album_art_color = None;
                self.current_art_url = None;
                self.art_attempts = 0;
                self.seeking = None;
            }

            Message::Polled(poll) => {
                self.poll_in_flight = false;
                // Assign only on change; each summary is three owned Strings and
                // most ticks report an identical list.
                if self.players != poll.players {
                    self.players = poll.players;
                }
                // Fall back to auto-pick if the pinned player is gone.
                if let Some(sel) = &self.selected_player {
                    if !self.players.iter().any(|p| &p.bus_name == sel) {
                        self.selected_player = None;
                    }
                }

                let Some(info) = poll.player else {
                    // Transient miss: keep the last-known player for a few ticks.
                    if !self.player.bus_name.is_empty() {
                        self.missed_polls = self.missed_polls.saturating_add(1);
                        if self.missed_polls >= 4 {
                            self.player = PlayerInfo::default();
                            self.album_art = None;
                            self.album_art_blurred = None;
                            self.album_art_color = None;
                            self.current_art_url = None;
                            self.art_attempts = 0;
                            self.seeking = None;
                        }
                    }
                    return Task::none();
                };

                self.missed_polls = 0;

                // Runs before the unchanged short-circuit, so it still fires for
                // players that report identical ticks.
                if let Some(target) = self.seeking {
                    if (info.position_us as f64 - target).abs() < 2_000_000.0 {
                        self.seeking = None;
                    }
                }

                // Independent of the rest of the state, so a retry still fires on
                // an otherwise-identical tick.
                let art_task = self.reconcile_art(info.art_url.as_deref());
                let unchanged = info == self.player;
                self.player = info;

                if let Some(task) = art_task {
                    return task;
                }
                if unchanged {
                    return Task::none();
                }
            }

            Message::ArtLoaded { url, handle, blurred, color } => {
                // Ignore art for a track we've moved past; on failure leave it
                // None so `reconcile_art` retries.
                if self.current_art_url.as_deref() == Some(url.as_str()) && handle.is_some() {
                    self.album_art = handle;
                    self.album_art_blurred = blurred;
                    self.album_art_color = color;
                }
            }

            Message::PlayPause => {
                let bus_name = self.player.bus_name.clone();
                tokio::task::spawn_blocking(move || crate::mpris::play_pause(&bus_name));
            }

            Message::Next => {
                let bus_name = self.player.bus_name.clone();
                tokio::task::spawn_blocking(move || crate::mpris::next(&bus_name));
            }

            Message::Previous => {
                let bus_name = self.player.bus_name.clone();
                tokio::task::spawn_blocking(move || crate::mpris::previous(&bus_name));
            }

            Message::Scroll(notches) => {
                // One skip per whole notch, keeping the remainder.
                self.scroll_accum += notches;
                let bus_name = self.player.bus_name.clone();
                while self.scroll_accum >= 1.0 {
                    self.scroll_accum -= 1.0;
                    let bus = bus_name.clone();
                    tokio::task::spawn_blocking(move || crate::mpris::next(&bus));
                }
                while self.scroll_accum <= -1.0 {
                    self.scroll_accum += 1.0;
                    let bus = bus_name.clone();
                    tokio::task::spawn_blocking(move || crate::mpris::previous(&bus));
                }
            }

            Message::SeekChanged(pos) => {
                self.seeking = Some(pos);
            }

            Message::SeekCommit => {
                if let Some(pos) = self.seeking {
                    // `seeking` stays set: the slider is pinned to the target until
                    // a poll confirms the position caught up.
                    let target = pos as u64;
                    let bus_name = self.player.bus_name.clone();
                    let current = self.player.position_us;
                    tokio::task::spawn_blocking(move || {
                        crate::mpris::seek_to(&bus_name, target, current)
                    });
                }
            }

            Message::LabelMaxLengthChanged(len) => {
                self.config.panel_label_max_length = len;
                if let Ok(ctx) = cosmic_config::Config::new(Self::APP_ID, Config::VERSION) {
                    let _ = self.config.write_entry(&ctx);
                }
            }

            Message::TrackFirstChanged(val) => {
                self.config.track_first = val;
                if let Ok(ctx) = cosmic_config::Config::new(Self::APP_ID, Config::VERSION) {
                    let _ = self.config.write_entry(&ctx);
                }
            }

        }

        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

impl AppModel {
    /// Dispatch one poll off the UI thread, marking a poll in flight so ticks
    /// don't stack. `popup_open` gates the popup-only extras (position/caps).
    fn spawn_poll(&mut self, popup_open: bool) -> Task<Message> {
        self.ticks_since_poll = 0;
        self.poll_in_flight = true;
        let selected = self.selected_player.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    crate::mpris::poll(selected.as_deref(), popup_open)
                })
                .await
                .unwrap_or_default()
            },
            |poll| cosmic::Action::App(Message::Polled(poll)),
        )
    }

    /// Start an art fetch when one is due: on a new URL, or as a retry (up to
    /// [`MAX_ART_ATTEMPTS`]) while the current URL still has no art.
    fn reconcile_art(&mut self, art_url: Option<&str>) -> Option<Task<Message>> {
        let Some(url) = art_url else {
            // Track exposes no art; clear any stale cover.
            self.current_art_url = None;
            self.album_art = None;
            self.album_art_blurred = None;
            self.album_art_color = None;
            self.art_attempts = 0;
            return None;
        };

        if self.current_art_url.as_deref() != Some(url) {
            // New track/URL: reset and start loading.
            self.current_art_url = Some(url.to_string());
            self.album_art = None;
            self.album_art_blurred = None;
            self.album_art_color = None;
            self.art_attempts = 0;
        } else if self.album_art.is_some() || self.art_attempts >= MAX_ART_ATTEMPTS {
            // Already loaded, or retries exhausted for this URL.
            return None;
        }

        self.art_attempts += 1;
        let url = url.to_string();
        Some(Task::perform(load_art(url.clone()), move |result| {
            let (handle, blurred, color) = match result {
                Some((h, b, c)) => (Some(h), Some(b), Some(c)),
                None => (None, None, None),
            };
            cosmic::Action::App(Message::ArtLoaded {
                url: url.clone(),
                handle,
                blurred,
                color,
            })
        }))
    }
}

fn format_time(us: u64) -> String {
    let secs = us / 1_000_000;
    let mins = secs / 60;
    format!("{}:{:02}", mins, secs % 60)
}

fn truncate_label(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}\u{2026}")
    } else {
        truncated
    }
}

/// Cover thumbnail size: crisp enough for the popup hero, and far smaller than
/// a full-resolution cover (~9MB of RGBA), which is paid once per monitor.
const THUMB_SIZE: u32 = 400;

/// Fixed popup width, so the hero cover is a square filling the inner width.
const POPUP_WIDTH: f32 = BACKDROP_SIZE.0 as f32;

/// Backdrop texture size. The height is nominal: the renderer stretches it to
/// the real popup, which only bends the baked corner arcs by the difference.
const BACKDROP_SIZE: (u32, u32) = (360, 560);

/// Shared look for controls on the scrim: translucent white fill, with text and
/// icons forced white rather than the theme's on-surface colour.
fn scrim_style(fill: f32, alpha: f32, radius: f32) -> cosmic::widget::button::Style {
    use cosmic::iced::{Background, Color, Vector};
    cosmic::widget::button::Style {
        shadow_offset: Vector::ZERO,
        background: (fill > 0.0)
            .then_some(Background::Color(Color { a: fill, ..Color::WHITE })),
        overlay: None,
        border_radius: radius.into(),
        border_width: 0.0,
        border_color: Color::TRANSPARENT,
        outline_width: 0.0,
        outline_color: Color::TRANSPARENT,
        icon_color: Some(Color { a: alpha, ..Color::WHITE }),
        text_color: Some(Color { a: alpha, ..Color::WHITE }),
    }
}

/// Transport and gear controls. The radius is large enough to keep the square
/// icon buttons circular at any size; the renderer clamps it to half the side.
fn scrim_button() -> cosmic::theme::Button {
    const R: f32 = 1000.0;
    cosmic::theme::Button::Custom {
        active: Box::new(|_focused, _theme| scrim_style(0.12, 1.0, R)),
        disabled: Box::new(|_theme| scrim_style(0.06, 0.4, R)),
        hovered: Box::new(|_focused, _theme| scrim_style(0.22, 1.0, R)),
        pressed: Box::new(|_focused, _theme| scrim_style(0.30, 1.0, R)),
    }
}

/// Player-picker rows. The selected row holds a brighter fill instead of a
/// theme accent, which would fight the cover behind it.
fn scrim_row_button(selected: bool) -> cosmic::theme::Button {
    let base = if selected { 0.26 } else { 0.0 };
    let radius = move |theme: &cosmic::Theme| theme.cosmic().corner_radii.radius_s[0];
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, theme| scrim_style(base, 1.0, radius(theme))),
        disabled: Box::new(move |theme| scrim_style(base, 0.4, radius(theme))),
        hovered: Box::new(move |_focused, theme| {
            scrim_style(base.max(0.16), 1.0, radius(theme))
        }),
        pressed: Box::new(move |_focused, theme| {
            scrim_style(base.max(0.30), 1.0, radius(theme))
        }),
    }
}

/// Resolution the cover is blurred at. Small enough to be free, and the blur
/// hides the upscale back to popup size.
const BLUR_SRC_SIZE: u32 = 48;

/// Gaussian sigma for the backdrop blur.
const BLUR_SIGMA: f32 = 6.0;

/// Cap on cached cover thumbnails (~130KB each → ~16MB). The shared cache is
/// keyed by URL, so this is per-unique-cover, not per-instance.
const ART_CACHE_MAX: usize = 128;

/// One shared HTTP client: building a client per fetch reconstructs a TLS stack
/// and connection pool and defeats keep-alive across the retry attempts.
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
});

/// (sharp cover, blurred backdrop, average cover colour rgba 0..1).
type ArtPair = (
    cosmic::iced::widget::image::Handle,
    cosmic::iced::widget::image::Handle,
    [f32; 4],
);

async fn load_art(url: String) -> Option<ArtPair> {
    let path = cache_path(&url);

    // Shared across instances: with one applet per monitor, the first to fetch a
    // cover writes a thumbnail the others just decode.
    if let Some(ref p) = path {
        if let Ok(bytes) = tokio::fs::read(p).await {
            if let Some(pair) =
                tokio::task::spawn_blocking(move || decode_rgba(&bytes)).await.ok().flatten()
            {
                return Some(pair);
            }
        }
    }

    let bytes = fetch_bytes(&url).await?;
    tokio::task::spawn_blocking(move || process_and_cache(&bytes, path.as_deref()))
        .await
        .ok()
        .flatten()
}

/// Fetch the raw encoded image bytes, from disk for `file://` URLs or over HTTP.
async fn fetch_bytes(url: &str) -> Option<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return tokio::fs::read(path).await.ok();
    }
    let resp = HTTP.get(url).send().await.ok()?;
    // Reject error pages (Jellyfin serves a JSON 404 while it generates a
    // variant) rather than feeding them to the decoder.
    if !resp.status().is_success() {
        return None;
    }
    resp.bytes().await.ok().map(|b| b.to_vec())
}

/// Directory holding cached cover thumbnails, shared across applet instances.
fn art_cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("cosmic-ext-applet-on-blast").join("art"))
}

/// Deterministic cache path for an art URL. `DefaultHasher` has a fixed seed, so
/// every applet instance maps the same URL to the same file.
fn cache_path(url: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    Some(art_cache_dir()?.join(format!("{:016x}.png", hasher.finish())))
}

/// Decode already-square cached bytes into (sharp, blurred-backdrop) handles.
fn decode_rgba(bytes: &[u8]) -> Option<ArtPair> {
    use cosmic::iced::widget::image::Handle;
    let img = image::load_from_memory(bytes).ok()?;
    let (blurred, color) = blur_backdrop(&img);
    let rgba = img.into_rgba8();
    let sharp = Handle::from_rgba(rgba.width(), rgba.height(), rgba.into_raw());
    Some((sharp, blurred, color))
}

/// Blur tiny (the blur hides the upscale), then stretch to the popup box — the
/// texture has to reach popup scale, because that's where the corners are cut.
fn blur_backdrop(img: &image::DynamicImage) -> (cosmic::iced::widget::image::Handle, [f32; 4]) {
    use cosmic::iced::widget::image::Handle;
    let small = img.resize_exact(
        BLUR_SRC_SIZE,
        BLUR_SRC_SIZE,
        image::imageops::FilterType::Triangle,
    );
    let blurred = image::imageops::blur(&small.to_rgba8(), BLUR_SIGMA);
    let color = average_color(&blurred);
    let (w, h) = BACKDROP_SIZE;
    let mut full = image::imageops::resize(&blurred, w, h, image::imageops::FilterType::Triangle);
    // Covers rebuild on every track change, so a radius tweak lands on the next.
    round_corners(
        &mut full,
        cosmic::theme::active().cosmic().corner_radii.radius_m[0],
    );
    let handle = Handle::from_rgba(w, h, full.into_raw());
    (handle, color)
}

/// Clear the alpha outside the corner arcs, feathered by a pixel. Not
/// `image::border_radius`: that rounds at texture scale and smears the arc.
fn round_corners(img: &mut image::RgbaImage, radius: f32) {
    let (w, h) = (img.width(), img.height());
    let r = radius.min(w as f32 / 2.0).min(h as f32 / 2.0);
    if r <= 0.0 {
        return;
    }
    let n = r.ceil() as u32;
    for cy in 0..n {
        for cx in 0..n {
            let (dx, dy) = (r - (cx as f32 + 0.5), r - (cy as f32 + 0.5));
            if dx <= 0.0 || dy <= 0.0 {
                continue;
            }
            let coverage = (0.5 - (dx.hypot(dy) - r)).clamp(0.0, 1.0);
            if coverage >= 1.0 {
                continue;
            }
            for (x, y) in [
                (cx, cy),
                (w - 1 - cx, cy),
                (w - 1 - cx, h - 1 - cy),
                (cx, h - 1 - cy),
            ] {
                let p = img.get_pixel_mut(x, y);
                p[3] = (f32::from(p[3]) * coverage) as u8;
            }
        }
    }
}

/// Mean rgb of the blurred cover, as an opaque rgba 0..1 tuple.
fn average_color(img: &image::RgbaImage) -> [f32; 4] {
    let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
    for p in img.pixels() {
        r += u64::from(p[0]);
        g += u64::from(p[1]);
        b += u64::from(p[2]);
    }
    let n = u64::from(img.width() * img.height()).max(1);
    [
        (r / n) as f32 / 255.0,
        (g / n) as f32 / 255.0,
        (b / n) as f32 / 255.0,
        1.0,
    ]
}

/// Decode, centre-crop to a square (players like Plexamp hand back banner art),
/// downscale to [`THUMB_SIZE`], and best-effort cache. `None` means try again.
fn process_and_cache(bytes: &[u8], cache: Option<&Path>) -> Option<ArtPair> {
    use cosmic::iced::widget::image::Handle;
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    let side = w.min(h);
    let square = img.crop_imm((w - side) / 2, (h - side) / 2, side, side);
    let thumb = if side > THUMB_SIZE {
        square.resize_exact(THUMB_SIZE, THUMB_SIZE, image::imageops::FilterType::Triangle)
    } else {
        square
    };

    // Only on a new cover, so the directory scan is off the hot path.
    if let Some(path) = cache {
        if write_png_atomic(&thumb, path).is_ok() {
            if let Some(dir) = path.parent() {
                prune_art_cache(dir, ART_CACHE_MAX);
            }
        }
    }

    let (blurred, color) = blur_backdrop(&thumb);
    let rgba = thumb.into_rgba8();
    let sharp = Handle::from_rgba(rgba.width(), rgba.height(), rgba.into_raw());
    Some((sharp, blurred, color))
}

/// Write `img` as a PNG via a per-process temp file + rename, so instances never
/// observe a partially-written cache entry.
fn write_png_atomic(img: &image::DynamicImage, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    img.save_with_format(&tmp, image::ImageFormat::Png)
        .map_err(std::io::Error::other)?;
    std::fs::rename(&tmp, path)
}

/// Cap the shared cache at `max` thumbnails, oldest deleted first. Best-effort,
/// and `.tmp` files (other instances' in-flight writes) are left alone.
fn prune_art_cache(dir: &Path, max: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut pngs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("png") {
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, path))
        })
        .collect();
    if pngs.len() <= max {
        return;
    }
    let excess = pngs.len() - max;
    pngs.sort_by_key(|(mtime, _)| *mtime); // oldest first
    for (_, path) in pngs.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_time_pads_seconds() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(5_000_000), "0:05");
        assert_eq!(format_time(65_000_000), "1:05");
        assert_eq!(format_time(600_000_000), "10:00");
    }

    #[test]
    fn truncate_label_appends_ellipsis_only_when_cut() {
        assert_eq!(truncate_label("hello", 10), "hello");
        assert_eq!(truncate_label("hello", 5), "hello");
        assert_eq!(truncate_label("hello world", 5), "hello\u{2026}");
    }

    #[test]
    fn truncate_label_counts_chars_not_bytes() {
        // Multi-byte chars count as one each and must not be split.
        assert_eq!(truncate_label("héllo wörld", 5), "héllo\u{2026}");
    }

    #[test]
    fn cache_path_is_stable_and_url_specific() {
        assert_eq!(cache_path("http://a/1.jpg"), cache_path("http://a/1.jpg"));
        assert_ne!(cache_path("http://a/1.jpg"), cache_path("http://a/2.jpg"));
    }

    #[test]
    fn prune_caps_pngs_and_ignores_tmp() {
        let dir =
            std::env::temp_dir().join(format!("np-prune-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..5 {
            std::fs::write(dir.join(format!("cover{i}.png")), b"x").unwrap();
        }
        // An in-flight temp write from another instance must survive pruning.
        std::fs::write(dir.join("cover9.99.tmp"), b"x").unwrap();

        prune_art_cache(&dir, 3);

        let pngs = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
            .count();
        let tmps = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .count();
        assert_eq!(pngs, 3, "pngs pruned to cap");
        assert_eq!(tmps, 1, "tmp files untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
