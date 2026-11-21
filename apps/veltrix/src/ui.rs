use crate::platform::Job;
#[cfg(target_os = "linux")]
use gpui_kit::WindowDecorations;
use gpui_kit::{
    App, Bounds, Context, FontWeight, Hsla, MouseButton, PathBuilder, Render, TitlebarOptions,
    Window, WindowAppearance, WindowBounds, WindowControlArea, WindowOptions, canvas, div, img,
    linear_color_stop, linear_gradient, point, prelude::*, px, relative, rgb, rgba, size,
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use veltrix_core::UpdateEvent;

const INDETERMINATE_WIDTH: f32 = 0.36;
const TICK_MS: u64 = 50;
const AUTO_CLOSE_TICKS: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhaseKind {
    Running,
    Restoring,
    Succeeded,
    RestoredAfterFailure,
    Failed,
}

#[derive(Debug)]
struct UiState {
    event: UpdateEvent,
}

impl UiState {
    fn new(event: UpdateEvent) -> Self {
        Self { event }
    }

    fn apply_event(&mut self, event: UpdateEvent) {
        self.event = event;
    }

    fn phase_kind(&self) -> PhaseKind {
        if self.event.terminal {
            if self.event.success {
                PhaseKind::Succeeded
            } else if self.event.message.contains("Se restauró la versión") {
                PhaseKind::RestoredAfterFailure
            } else {
                PhaseKind::Failed
            }
        } else if self.event.stage == "Restaurando instalación" {
            PhaseKind::Restoring
        } else {
            PhaseKind::Running
        }
    }

    fn headline(&self) -> &'static str {
        match self.phase_kind() {
            PhaseKind::Running => "Aplicando actualización...",
            PhaseKind::Restoring => "Restaurando versión anterior...",
            PhaseKind::Succeeded => "Actualización completada",
            PhaseKind::RestoredAfterFailure => "Se restauró la versión anterior",
            PhaseKind::Failed => "No se pudo actualizar",
        }
    }

    fn subtitle(&self) -> String {
        match self.phase_kind() {
            PhaseKind::Running => "Espera un momento mientras se instala la actualización.".into(),
            PhaseKind::Restoring => {
                "Se detectó un problema y se está restaurando la versión anterior.".into()
            }
            PhaseKind::Succeeded => "La nueva versión ya está lista.".into(),
            PhaseKind::RestoredAfterFailure | PhaseKind::Failed => self.event.message.clone(),
        }
    }

    fn status_line(&self) -> String {
        match self.phase_kind() {
            PhaseKind::Succeeded => "Cerrando actualizador...".into(),
            PhaseKind::RestoredAfterFailure => "Versión anterior restaurada.".into(),
            PhaseKind::Failed => "Actualización fallida.".into(),
            PhaseKind::Restoring => "Restaurando respaldo firmado...".into(),
            PhaseKind::Running => match self.event.stage.as_str() {
                "Preparando actualización" | "Respaldo no disponible" => {
                    "Preparando actualización...".into()
                }
                "Verificando integridad" => {
                    if let Some(fraction) = self.progress_fraction() {
                        let percent = (fraction * 100.0).round() as u8;
                        format!("Copiando instalador... {percent} %")
                    } else if self.event.message.contains("SHA-256") {
                        "Comprobando integridad SHA-256...".into()
                    } else {
                        "Verificando integridad...".into()
                    }
                }
                "Esperando cierre de la aplicación" => {
                    "Esperando cierre de la aplicación...".into()
                }
                "Instalando actualización" => "Instalando actualización...".into(),
                "Validando instalación" => "Validando instalación...".into(),
                "Reiniciando aplicación" => {
                    if self.event.message.contains("confirmación") {
                        "Esperando confirmación de arranque...".into()
                    } else {
                        "Reiniciando aplicación...".into()
                    }
                }
                other => format!("{other}..."),
            },
        }
    }

    fn progress_fraction(&self) -> Option<f32> {
        if self.event.terminal {
            return None;
        }
        let (done, total) = self.event.progress?;
        if total == 0 {
            return None;
        }
        Some((done as f32 / total as f32).clamp(0.0, 1.0))
    }
}

fn should_auto_close(finished: bool, succeeded: bool, finish_ticks: u8) -> bool {
    finished && succeeded && finish_ticks >= AUTO_CLOSE_TICKS
}

#[derive(Clone, Copy)]
struct Palette {
    bg: Hsla,
    titlebar_bg: Hsla,
    titlebar_border: Hsla,
    text_primary: Hsla,
    text_secondary: Hsla,
    version_pill_bg: Hsla,
    version_pill_text: Hsla,
    track_bg: Hsla,
    bar_start: Hsla,
    bar_end: Hsla,
    spinner_track: Hsla,
    control_fg: Hsla,
    control_disabled: Hsla,
    control_hover: Hsla,
    close_hover: Hsla,
    close_hover_fg: Hsla,
    success_start: Hsla,
    success_end: Hsla,
    restore_start: Hsla,
    restore_end: Hsla,
    error_start: Hsla,
    error_end: Hsla,
    button_bg: Hsla,
    button_hover: Hsla,
    button_border: Hsla,
}

impl Palette {
    fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
    }

    fn dark() -> Self {
        Self {
            bg: rgb(0x131a26).into(),
            titlebar_bg: rgb(0x182130).into(),
            titlebar_border: rgba(0xffffff14).into(),
            text_primary: rgb(0xf8fafc).into(),
            text_secondary: rgb(0xa3b2c7).into(),
            version_pill_bg: rgb(0x243044).into(),
            version_pill_text: rgb(0xcbd5e1).into(),
            track_bg: rgb(0x253247).into(),
            bar_start: rgb(0x1d68ff).into(),
            bar_end: rgb(0x00d2ff).into(),
            spinner_track: rgba(0x94a3b833).into(),
            control_fg: rgb(0xcbd5e1).into(),
            control_disabled: rgba(0x94a3b840).into(),
            control_hover: rgba(0xffffff14).into(),
            close_hover: rgb(0xe11d48).into(),
            close_hover_fg: rgb(0xffffff).into(),
            success_start: rgb(0x16a34a).into(),
            success_end: rgb(0x22c55e).into(),
            restore_start: rgb(0xd97706).into(),
            restore_end: rgb(0xfbbf24).into(),
            error_start: rgb(0xdc2626).into(),
            error_end: rgb(0xf87171).into(),
            button_bg: rgb(0x243044).into(),
            button_hover: rgb(0x31415c).into(),
            button_border: rgba(0xffffff1a).into(),
        }
    }

    fn light() -> Self {
        Self {
            bg: rgb(0xf7f9fc).into(),
            titlebar_bg: rgb(0xf4f7fc).into(),
            titlebar_border: rgba(0x0f172a12).into(),
            text_primary: rgb(0x0f172a).into(),
            text_secondary: rgb(0x4b5563).into(),
            version_pill_bg: rgb(0xe7eef9).into(),
            version_pill_text: rgb(0x1e293b).into(),
            track_bg: rgb(0xdce4f2).into(),
            bar_start: rgb(0x1d68ff).into(),
            bar_end: rgb(0x00c8ff).into(),
            spinner_track: rgba(0x64748b33).into(),
            control_fg: rgb(0x1e293b).into(),
            control_disabled: rgba(0x64748b47).into(),
            control_hover: rgba(0x0f172a0f).into(),
            close_hover: rgb(0xe11d48).into(),
            close_hover_fg: rgb(0xffffff).into(),
            success_start: rgb(0x16a34a).into(),
            success_end: rgb(0x22c55e).into(),
            restore_start: rgb(0xd97706).into(),
            restore_end: rgb(0xf59e0b).into(),
            error_start: rgb(0xdc2626).into(),
            error_end: rgb(0xef4444).into(),
            button_bg: rgb(0xe7eef9).into(),
            button_hover: rgb(0xdbe6f7).into(),
            button_border: rgba(0x0f172a14).into(),
        }
    }
}

fn indeterminate_offset(tick: usize, bar_width: f32) -> f32 {
    let period = 44.0_f32;
    let phase = (tick as f32 % period) / period;
    let wave = 0.5 - 0.5 * (phase * std::f32::consts::TAU).cos();
    wave * (1.0 - bar_width)
}

struct View {
    name: String,
    version_pill: String,
    icon: Option<PathBuf>,
    state: Arc<Mutex<UiState>>,
    can_close: Arc<AtomicBool>,
    tick: usize,
}

impl View {
    fn render_app_icon(&self, dimension: f32, radius: f32, glyph_size: f32) -> impl IntoElement {
        let base = div()
            .size(px(dimension))
            .rounded(px(radius))
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0();

        match &self.icon {
            Some(path) => base.child(img(path.clone()).size(px(dimension))),
            None => base
                .bg(linear_gradient(
                    135.,
                    linear_color_stop(rgb(0x1d68ff), 0.),
                    linear_color_stop(rgb(0x00b4ff), 1.),
                ))
                .text_color(rgb(0xffffff))
                .text_size(px(glyph_size))
                .font_weight(FontWeight::BOLD)
                .child("↑"),
        }
    }

    fn render_titlebar(&self, palette: Palette, closable: bool) -> impl IntoElement {
        let can_close_flag = self.can_close.clone();

        let close_btn = div()
            .id("window-close")
            .w(px(38.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.))
            .text_color(if closable {
                palette.control_fg
            } else {
                palette.control_disabled
            })
            .when(closable, |el| {
                el.cursor_pointer()
                    .hover(move |s| s.bg(palette.close_hover).text_color(palette.close_hover_fg))
                    .on_mouse_down(MouseButton::Left, move |_, window, _| {
                        if can_close_flag.load(Ordering::Acquire) {
                            window.remove_window();
                        }
                    })
            })
            .child("✕");

        div()
            .w_full()
            .h(px(44.))
            .bg(palette.titlebar_bg)
            .border_b_1()
            .border_color(palette.titlebar_border)
            .flex()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .pl(px(14.))
                    .pr(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .window_control_area(WindowControlArea::Drag)
                    .child(self.render_app_icon(22., 6., 12.))
                    .child(
                        div()
                            .text_size(px(13.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(palette.text_primary)
                            .whitespace_nowrap()
                            .child(self.name.clone()),
                    )
                    .child(
                        div()
                            .px(px(10.))
                            .py(px(3.))
                            .rounded_full()
                            .bg(palette.version_pill_bg)
                            .text_size(px(11.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(palette.version_pill_text)
                            .whitespace_nowrap()
                            .child(self.version_pill.clone()),
                    ),
            )
            .child(
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .child(
                        div()
                            .id("window-minimize")
                            .w(px(38.))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .text_size(px(13.))
                            .text_color(palette.control_fg)
                            .hover(move |s| s.bg(palette.control_hover))
                            .on_mouse_down(MouseButton::Left, |_, window, _| {
                                window.minimize_window();
                            })
                            .child("─"),
                    )
                    .child(close_btn),
            )
    }

    fn render_progress(&self, state: &UiState, palette: Palette) -> impl IntoElement {
        let phase = state.phase_kind();
        let fraction = state.progress_fraction();

        let (start, end) = match phase {
            PhaseKind::Running => (palette.bar_start, palette.bar_end),
            PhaseKind::Restoring | PhaseKind::RestoredAfterFailure => {
                (palette.restore_start, palette.restore_end)
            }
            PhaseKind::Succeeded => (palette.success_start, palette.success_end),
            PhaseKind::Failed => (palette.error_start, palette.error_end),
        };

        let (offset, width) = match (phase, fraction) {
            (PhaseKind::Succeeded | PhaseKind::RestoredAfterFailure | PhaseKind::Failed, _) => {
                (0.0_f32, 1.0_f32)
            }
            (_, Some(f)) => (0.0_f32, f.max(0.04)),
            (_, None) => (
                indeterminate_offset(self.tick, INDETERMINATE_WIDTH),
                INDETERMINATE_WIDTH,
            ),
        };

        div()
            .w_full()
            .h(px(10.))
            .bg(palette.track_bg)
            .rounded_full()
            .overflow_hidden()
            .child(
                div()
                    .ml(relative(offset))
                    .w(relative(width))
                    .h_full()
                    .rounded_full()
                    .bg(linear_gradient(
                        90.,
                        linear_color_stop(start, 0.),
                        linear_color_stop(end, 1.),
                    )),
            )
    }

    fn render_spinner(&self, phase: PhaseKind, palette: Palette) -> impl IntoElement {
        let tick = self.tick;
        let (arc_start, arc_end) = match phase {
            PhaseKind::Restoring => (palette.restore_start, palette.restore_end),
            _ => (palette.bar_start, palette.bar_end),
        };
        let track_color = palette.spinner_track;

        canvas(
            |_, _, _| (),
            move |bounds, (), window, _| {
                let cx = f32::from(bounds.origin.x) + f32::from(bounds.size.width) * 0.5;
                let cy = f32::from(bounds.origin.y) + f32::from(bounds.size.height) * 0.5;
                let radius =
                    (f32::from(bounds.size.width).min(f32::from(bounds.size.height)) * 0.5 - 2.0)
                        .max(2.0);
                let radii = point(px(radius), px(radius));

                let mut track = PathBuilder::stroke(px(2.2));
                track.move_to(point(px(cx + radius), px(cy)));
                track.arc_to(radii, px(0.), false, true, point(px(cx - radius), px(cy)));
                track.arc_to(radii, px(0.), false, true, point(px(cx + radius), px(cy)));
                if let Ok(path) = track.build() {
                    window.paint_path(path, track_color);
                }

                let start_angle = (tick as f32) * 0.24;
                let sweep_angle = 2.45_f32;
                let end_angle = start_angle + sweep_angle;
                let p0 = point(
                    px(cx + radius * start_angle.cos()),
                    px(cy + radius * start_angle.sin()),
                );
                let p1 = point(
                    px(cx + radius * end_angle.cos()),
                    px(cy + radius * end_angle.sin()),
                );

                let mut arc = PathBuilder::stroke(px(2.4));
                arc.move_to(p0);
                arc.arc_to(radii, px(0.), false, true, p1);
                if let Ok(path) = arc.build() {
                    window.paint_path(
                        path,
                        linear_gradient(
                            135.,
                            linear_color_stop(arc_start, 0.),
                            linear_color_stop(arc_end, 1.),
                        ),
                    );
                }
            },
        )
        .size(px(18.))
        .flex_shrink_0()
    }

    fn render_status_indicator(&self, phase: PhaseKind, palette: Palette) -> impl IntoElement {
        match phase {
            PhaseKind::Running | PhaseKind::Restoring => {
                self.render_spinner(phase, palette).into_any_element()
            }
            PhaseKind::Succeeded => div()
                .size(px(18.))
                .rounded_full()
                .bg(palette.success_start)
                .text_color(rgb(0xffffff))
                .text_size(px(11.))
                .font_weight(FontWeight::BOLD)
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .child("✓")
                .into_any_element(),
            PhaseKind::RestoredAfterFailure => div()
                .size(px(18.))
                .rounded_full()
                .bg(palette.restore_start)
                .text_color(rgb(0xffffff))
                .text_size(px(11.))
                .font_weight(FontWeight::BOLD)
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .child("↺")
                .into_any_element(),
            PhaseKind::Failed => div()
                .size(px(18.))
                .rounded_full()
                .bg(palette.error_start)
                .text_color(rgb(0xffffff))
                .text_size(px(11.))
                .font_weight(FontWeight::BOLD)
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .child("✕")
                .into_any_element(),
        }
    }
}

impl Render for View {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_appearance(window.appearance());
        let state = self.state.lock().expect("state mutex");
        let phase = state.phase_kind();
        let closable = self.can_close.load(Ordering::Acquire);

        let title_color = match phase {
            PhaseKind::Running => palette.text_primary,
            PhaseKind::Restoring | PhaseKind::RestoredAfterFailure => palette.restore_end,
            PhaseKind::Succeeded => palette.success_end,
            PhaseKind::Failed => palette.error_end,
        };

        div()
            .size_full()
            .overflow_hidden()
            .bg(palette.bg)
            .text_color(palette.text_primary)
            .flex()
            .flex_col()
            .child(self.render_titlebar(palette, closable))
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .px(px(24.))
                    .py(px(20.))
                    .flex()
                    .flex_col()
                    .justify_between()
                    .gap(px(16.))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(16.))
                            .child(self.render_app_icon(56., 14., 26.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.))
                                    .child(
                                        div()
                                            .text_size(px(20.))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(title_color)
                                            .child(state.headline()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(13.))
                                            .text_color(palette.text_secondary)
                                            .child(state.subtitle()),
                                    ),
                            ),
                    )
                    .child(self.render_progress(&state, palette))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(10.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .child(self.render_status_indicator(phase, palette))
                                    .child(
                                        div()
                                            .text_size(px(13.))
                                            .text_color(palette.text_secondary)
                                            .child(state.status_line()),
                                    ),
                            )
                            .when(
                                state.event.terminal && !state.event.success && closable,
                                |footer| {
                                    footer.child(
                                        div()
                                            .id("close-terminal-button")
                                            .px(px(14.))
                                            .py(px(5.))
                                            .rounded_full()
                                            .bg(palette.button_bg)
                                            .border_1()
                                            .border_color(palette.button_border)
                                            .text_size(px(12.))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(palette.text_primary)
                                            .cursor_pointer()
                                            .flex_shrink_0()
                                            .hover(move |s| s.bg(palette.button_hover))
                                            .on_mouse_down(MouseButton::Left, |_, window, _| {
                                                window.remove_window();
                                            })
                                            .child("Cerrar"),
                                    )
                                },
                            ),
                    ),
            )
    }
}

fn window_options(title: &str, cx: &mut App) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(470.), px(248.)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(title.to_owned().into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        #[cfg(target_os = "linux")]
        window_decorations: Some(WindowDecorations::Client),
        is_resizable: false,
        is_minimizable: true,
        ..Default::default()
    }
}

pub fn run(job: Job) -> anyhow::Result<bool> {
    let result = Arc::new(AtomicBool::new(false));
    let worker_result = result.clone();
    let auto_close_result = result.clone();
    let failure = Arc::new(Mutex::new(None));
    let window_failure = failure.clone();
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        let state = Arc::new(Mutex::new(UiState::new(UpdateEvent {
            stage: "Preparando actualización".into(),
            message: "Comprobando el paquete y la configuración local.".into(),
            terminal: false,
            success: false,
            progress: None,
        })));
        let finished = Arc::new(AtomicBool::new(false));
        let can_close = finished.clone();
        let view_name = job.name.clone();
        let view = View {
            name: job.name.clone(),
            version_pill: format!("v{} → v{}", job.current, job.target),
            icon: job.icon.clone(),
            state: state.clone(),
            can_close: finished.clone(),
            tick: 0,
        };
        let options = window_options(&view_name, cx);
        let window = cx.open_window(options, |window, cx| {
            window.set_window_title(&view_name);
            cx.new(|cx| {
                cx.observe_window_appearance(window, |_, _, cx| cx.notify())
                    .detach();
                view
            })
        });
        let Ok(window) = window else {
            *window_failure.lock().expect("failure mutex") = window.err();
            cx.quit();
            return;
        };
        let worker_finished = finished.clone();
        let worker = std::thread::spawn(move || {
            let ok = job.run(|status| {
                state.lock().expect("state mutex").apply_event(UpdateEvent {
                    stage: status.stage.clone(),
                    message: status.message.clone(),
                    terminal: status.terminal,
                    success: status.success,
                    progress: status.progress,
                });
            });
            worker_result.store(ok, Ordering::Release);
            worker_finished.store(true, Ordering::Release);
        });
        let _ = window.update(cx, |_, window, cx| {
            window.on_window_should_close(cx, move |_, _| can_close.load(Ordering::Acquire));
        });
        let auto_close_finished = finished.clone();
        cx.spawn(async move |cx| {
            let mut finish_ticks: u8 = 0;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(TICK_MS))
                    .await;
                let is_finished = auto_close_finished.load(Ordering::Acquire);
                let is_success = auto_close_result.load(Ordering::Acquire);
                if is_finished && is_success {
                    finish_ticks = finish_ticks.saturating_add(1);
                }
                let close_now = should_auto_close(is_finished, is_success, finish_ticks);
                let updated = window.update(cx, |view, window, cx| {
                    view.tick = view.tick.wrapping_add(1);
                    cx.notify();
                    if close_now {
                        window.remove_window();
                    }
                });
                if updated.is_err() || close_now || (is_finished && !is_success) {
                    break;
                }
            }
        })
        .detach();
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        // The worker owns the lock and process handles through installation.
        drop(worker);
        cx.activate(true);
    });
    if let Some(error) = failure.lock().expect("failure mutex").take() {
        return Err(error);
    }
    Ok(result.load(Ordering::Acquire))
}

pub fn show_preparation_error(message: String) {
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        let state = Arc::new(Mutex::new(UiState::new(UpdateEvent {
            stage: "Actualización fallida".into(),
            message,
            terminal: true,
            success: false,
            progress: None,
        })));
        let can_close = Arc::new(AtomicBool::new(true));
        let options = window_options("Veltrix", cx);
        let result = cx.open_window(options, |window, cx| {
            window.set_window_title("Veltrix");
            cx.new(|cx| {
                cx.observe_window_appearance(window, |_, _, cx| cx.notify())
                    .detach();
                View {
                    name: "Veltrix".into(),
                    version_pill: "Error de preparación".into(),
                    icon: None,
                    state,
                    can_close,
                    tick: 0,
                }
            })
        });
        if result.is_err() {
            cx.quit();
            return;
        }
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(stage: &str, message: &str, terminal: bool, success: bool) -> UpdateEvent {
        UpdateEvent {
            stage: stage.into(),
            message: message.into(),
            terminal,
            success,
            progress: None,
        }
    }

    #[test]
    fn formats_status_lines_for_veltrix_stages() {
        let mut state = UiState::new(event("Preparando actualización", "Inicio", false, false));
        assert_eq!(state.phase_kind(), PhaseKind::Running);
        assert_eq!(state.headline(), "Aplicando actualización...");
        assert_eq!(state.status_line(), "Preparando actualización...");

        state.apply_event(UpdateEvent {
            stage: "Verificando integridad".into(),
            message: "Copiando el instalador.".into(),
            terminal: false,
            success: false,
            progress: Some((45, 100)),
        });
        assert_eq!(state.status_line(), "Copiando instalador... 45 %");

        state.apply_event(event(
            "Verificando integridad",
            "Comprobando SHA-256.",
            false,
            false,
        ));
        assert_eq!(state.status_line(), "Comprobando integridad SHA-256...");

        state.apply_event(event(
            "Esperando cierre de la aplicación",
            "Cierra la aplicación.",
            false,
            false,
        ));
        assert_eq!(state.status_line(), "Esperando cierre de la aplicación...");

        state.apply_event(event(
            "Instalando actualización",
            "Ejecutando instalador.",
            false,
            false,
        ));
        assert_eq!(state.status_line(), "Instalando actualización...");

        state.apply_event(event("Validando instalación", "Validando.", false, false));
        assert_eq!(state.status_line(), "Validando instalación...");

        state.apply_event(event(
            "Reiniciando aplicación",
            "Esperando la confirmación de arranque.",
            false,
            false,
        ));
        assert_eq!(state.status_line(), "Esperando confirmación de arranque...");

        state.apply_event(event("Actualización completada", "Listo.", true, true));
        assert_eq!(state.phase_kind(), PhaseKind::Succeeded);
        assert_eq!(state.status_line(), "Cerrando actualizador...");
    }

    #[test]
    fn distinguishes_restoring_restored_and_failed_states() {
        let mut state = UiState::new(event(
            "Restaurando instalación",
            "Aplicando respaldo.",
            false,
            false,
        ));
        assert_eq!(state.phase_kind(), PhaseKind::Restoring);
        assert_eq!(state.headline(), "Restaurando versión anterior...");
        assert_eq!(state.status_line(), "Restaurando respaldo firmado...");

        state.apply_event(event(
            "Actualización fallida",
            "NSIS falló. Se restauró la versión 1.0.0.",
            true,
            false,
        ));
        assert_eq!(state.phase_kind(), PhaseKind::RestoredAfterFailure);
        assert_eq!(state.headline(), "Se restauró la versión anterior");
        assert!(state.subtitle().contains("Se restauró la versión 1.0.0."));

        state.apply_event(event("Actualización fallida", "Error fatal.", true, false));
        assert_eq!(state.phase_kind(), PhaseKind::Failed);
        assert_eq!(state.headline(), "No se pudo actualizar");
        assert_eq!(state.subtitle(), "Error fatal.");
    }

    #[test]
    fn auto_closes_only_after_successful_completion() {
        assert!(!should_auto_close(false, false, AUTO_CLOSE_TICKS));
        assert!(!should_auto_close(false, true, AUTO_CLOSE_TICKS));
        assert!(!should_auto_close(true, false, AUTO_CLOSE_TICKS));
        assert!(!should_auto_close(true, true, AUTO_CLOSE_TICKS - 1));
        assert!(should_auto_close(true, true, AUTO_CLOSE_TICKS));
    }

    #[test]
    fn exposes_percentage_only_with_real_progress() {
        let mut state = UiState::new(event("Verificando integridad", "Copiando", false, false));
        assert_eq!(state.progress_fraction(), None);

        state.apply_event(UpdateEvent {
            stage: "Verificando integridad".into(),
            message: "Copiando".into(),
            terminal: false,
            success: false,
            progress: Some((75, 100)),
        });
        assert_eq!(state.progress_fraction(), Some(0.75));

        state.apply_event(UpdateEvent {
            stage: "Verificando integridad".into(),
            message: "Zero total".into(),
            terminal: false,
            success: false,
            progress: Some((0, 0)),
        });
        assert_eq!(state.progress_fraction(), None);

        state.apply_event(UpdateEvent {
            stage: "Actualización completada".into(),
            message: "OK".into(),
            terminal: true,
            success: true,
            progress: Some((100, 100)),
        });
        assert_eq!(state.progress_fraction(), None);
    }

    #[test]
    fn indeterminate_offset_stays_within_track_bounds() {
        for tick in 0..200 {
            let offset = indeterminate_offset(tick, INDETERMINATE_WIDTH);
            assert!(offset >= 0.0);
            assert!(offset + INDETERMINATE_WIDTH <= 1.0 + f32::EPSILON);
        }
    }

    #[test]
    fn selects_dark_and_light_palettes_by_appearance() {
        let dark = Palette::for_appearance(WindowAppearance::Dark);
        let vibrant_dark = Palette::for_appearance(WindowAppearance::VibrantDark);
        let light = Palette::for_appearance(WindowAppearance::Light);
        let vibrant_light = Palette::for_appearance(WindowAppearance::VibrantLight);

        assert_eq!(dark.bg, vibrant_dark.bg);
        assert_eq!(light.bg, vibrant_light.bg);
        assert_ne!(dark.bg, light.bg);
    }
}
