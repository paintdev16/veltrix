use crate::windows::Job;
use gpui_kit::{
    App, Bounds, Context, FontWeight, Hsla, MouseButton, Render, TitlebarOptions, Window,
    WindowAppearance, WindowBounds, WindowControlArea, WindowOptions, div, img, linear_color_stop,
    linear_gradient, prelude::*, px, relative, rgb, rgba, size,
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

const STEPS: [&str; 5] = [
    "Preparar",
    "Cerrar aplicación",
    "Aplicar",
    "Verificar",
    "Reiniciar",
];

const INDETERMINATE_WIDTH: f32 = 0.28;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhaseKind {
    Running,
    Restoring,
    Succeeded,
    RestoredAfterFailure,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StepVisual {
    Completed,
    Active,
    Pending,
    Restoring,
    Failed,
}

#[derive(Debug)]
struct UiState {
    event: UpdateEvent,
    active_step: usize,
}

impl UiState {
    fn new(event: UpdateEvent) -> Self {
        let mut state = Self {
            event,
            active_step: 0,
        };
        state.sync_step_from_stage();
        state
    }

    fn apply_event(&mut self, event: UpdateEvent) {
        self.event = event;
        self.sync_step_from_stage();
    }

    fn sync_step_from_stage(&mut self) {
        match self.event.stage.as_str() {
            "Preparando actualización" | "Respaldo no disponible" | "Verificando integridad" => {
                self.active_step = 0;
            }
            "Esperando cierre de la aplicación" => self.active_step = 1,
            "Instalando actualización" => self.active_step = 2,
            "Validando instalación" => self.active_step = 3,
            "Reiniciando aplicación" => self.active_step = 4,
            "Actualización completada" => self.active_step = STEPS.len(),
            _ => {
                if self.event.terminal && self.event.success {
                    self.active_step = STEPS.len();
                } else {
                    self.active_step = self.active_step.min(STEPS.len() - 1);
                }
            }
        }
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
            PhaseKind::Running => "Aplicando actualización…",
            PhaseKind::Restoring => "Restaurando versión anterior…",
            PhaseKind::Succeeded => "Actualización completada",
            PhaseKind::RestoredAfterFailure => "Se restauró la versión anterior",
            PhaseKind::Failed => "No se pudo completar la actualización",
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

    fn step_visual(&self, index: usize) -> StepVisual {
        match self.phase_kind() {
            PhaseKind::Succeeded => StepVisual::Completed,
            PhaseKind::Running => {
                if index < self.active_step {
                    StepVisual::Completed
                } else if index == self.active_step {
                    StepVisual::Active
                } else {
                    StepVisual::Pending
                }
            }
            PhaseKind::Restoring | PhaseKind::RestoredAfterFailure => {
                if index < self.active_step {
                    StepVisual::Completed
                } else if index == self.active_step {
                    StepVisual::Restoring
                } else {
                    StepVisual::Pending
                }
            }
            PhaseKind::Failed => {
                if index < self.active_step {
                    StepVisual::Completed
                } else if index == self.active_step {
                    StepVisual::Failed
                } else {
                    StepVisual::Pending
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Palette {
    bg_start: Hsla,
    bg_end: Hsla,
    glow_bg: Hsla,
    glow_border: Hsla,
    titlebar_bg: Hsla,
    titlebar_border: Hsla,
    divider: Hsla,
    text_primary: Hsla,
    text_secondary: Hsla,
    text_muted: Hsla,
    version_pill_bg: Hsla,
    version_pill_border: Hsla,
    version_from: Hsla,
    version_to_bg: Hsla,
    version_to_text: Hsla,
    track_bg: Hsla,
    bar_start: Hsla,
    bar_end: Hsla,
    percent_pill_bg: Hsla,
    percent_pill_border: Hsla,
    percent_pill_text: Hsla,
    step_done_bg: Hsla,
    step_done_fg: Hsla,
    step_active_ring: Hsla,
    step_active_core: Hsla,
    step_active_halo: Hsla,
    step_pending_border: Hsla,
    step_line_done: Hsla,
    step_line_pending: Hsla,
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
            bg_start: rgb(0x0b1220).into(),
            bg_end: rgb(0x0e172a).into(),
            glow_bg: rgba(0x1d4ed826).into(),
            glow_border: rgba(0x3b82f63d).into(),
            titlebar_bg: rgba(0x111a2de6).into(),
            titlebar_border: rgba(0xffffff14).into(),
            divider: rgba(0xffffff1f).into(),
            text_primary: rgb(0xf8fafc).into(),
            text_secondary: rgb(0x94a3b8).into(),
            text_muted: rgb(0x64748b).into(),
            version_pill_bg: rgba(0x1e293bc7).into(),
            version_pill_border: rgba(0x94a3b824).into(),
            version_from: rgb(0xcbd5e1).into(),
            version_to_bg: rgba(0x2563eb29).into(),
            version_to_text: rgb(0x38bdf8).into(),
            track_bg: rgb(0x1f2b3e).into(),
            bar_start: rgb(0x1d68ff).into(),
            bar_end: rgb(0x00d2ff).into(),
            percent_pill_bg: rgba(0x16243bf0).into(),
            percent_pill_border: rgba(0x38bdf833).into(),
            percent_pill_text: rgb(0x38bdf8).into(),
            step_done_bg: rgb(0x16a34a).into(),
            step_done_fg: rgb(0xffffff).into(),
            step_active_ring: rgb(0x3b82f6).into(),
            step_active_core: rgb(0x2563eb).into(),
            step_active_halo: rgba(0x2563eb33).into(),
            step_pending_border: rgb(0x475569).into(),
            step_line_done: rgb(0x334155).into(),
            step_line_pending: rgb(0x243044).into(),
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
            button_bg: rgba(0x1e293bf0).into(),
            button_hover: rgba(0x334155f0).into(),
            button_border: rgba(0xffffff1a).into(),
        }
    }

    fn light() -> Self {
        Self {
            bg_start: rgb(0xf9fbff).into(),
            bg_end: rgb(0xf1f6ff).into(),
            glow_bg: rgba(0x93c5fd38).into(),
            glow_border: rgba(0x93c5fd4d).into(),
            titlebar_bg: rgba(0xf5f8ffeb).into(),
            titlebar_border: rgba(0x0f172a12).into(),
            divider: rgba(0x0f172a1a).into(),
            text_primary: rgb(0x0f172a).into(),
            text_secondary: rgb(0x475569).into(),
            text_muted: rgb(0x64748b).into(),
            version_pill_bg: rgb(0xedf2fc).into(),
            version_pill_border: rgba(0x2563eb1a).into(),
            version_from: rgb(0x334155).into(),
            version_to_bg: rgba(0x2563eb1c).into(),
            version_to_text: rgb(0x1d4ed8).into(),
            track_bg: rgb(0xe2e8f0).into(),
            bar_start: rgb(0x1d68ff).into(),
            bar_end: rgb(0x00c8ff).into(),
            percent_pill_bg: rgb(0xe8f0fe).into(),
            percent_pill_border: rgba(0x2563eb1f).into(),
            percent_pill_text: rgb(0x1d4ed8).into(),
            step_done_bg: rgb(0x16a34a).into(),
            step_done_fg: rgb(0xffffff).into(),
            step_active_ring: rgb(0x1d4ed8).into(),
            step_active_core: rgb(0x1d4ed8).into(),
            step_active_halo: rgba(0x2563eb26).into(),
            step_pending_border: rgb(0xcbd5e1).into(),
            step_line_done: rgb(0xb8c7dc).into(),
            step_line_pending: rgb(0xdbe4f0).into(),
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
            button_bg: rgb(0xeff4fc).into(),
            button_hover: rgb(0xe2ebfa).into(),
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

#[derive(Clone)]
enum VersionBadge {
    Transition { from: String, to: String },
    Note(String),
}

struct View {
    name: String,
    versions: VersionBadge,
    icon: Option<PathBuf>,
    state: Arc<Mutex<UiState>>,
    can_close: Arc<AtomicBool>,
    tick: usize,
}

impl View {
    fn render_app_icon(&self) -> impl IntoElement {
        let base = div()
            .size(px(26.))
            .rounded(px(7.))
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0();

        match &self.icon {
            Some(path) => base.child(img(path.clone()).size(px(26.))),
            None => base
                .bg(linear_gradient(
                    135.,
                    linear_color_stop(rgb(0x1d68ff), 0.),
                    linear_color_stop(rgb(0x00b4ff), 1.),
                ))
                .text_color(rgb(0xffffff))
                .text_size(px(14.))
                .font_weight(FontWeight::BOLD)
                .child("↑"),
        }
    }

    fn render_version_pill(&self, palette: Palette) -> impl IntoElement {
        let pill = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(10.))
            .py(px(3.))
            .rounded_full()
            .bg(palette.version_pill_bg)
            .border_1()
            .border_color(palette.version_pill_border)
            .text_size(px(12.));

        match &self.versions {
            VersionBadge::Transition { from, to } => pill
                .child(
                    div()
                        .text_color(palette.version_from)
                        .font_weight(FontWeight::MEDIUM)
                        .child(from.clone()),
                )
                .child(div().text_color(palette.text_muted).child("→"))
                .child(
                    div()
                        .px(px(6.))
                        .py(px(1.))
                        .rounded_full()
                        .bg(palette.version_to_bg)
                        .text_color(palette.version_to_text)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(to.clone()),
                ),
            VersionBadge::Note(note) => pill
                .text_color(palette.text_secondary)
                .font_weight(FontWeight::MEDIUM)
                .child(note.clone()),
        }
    }

    fn render_titlebar(&self, palette: Palette, closable: bool) -> impl IntoElement {
        let can_close_flag = self.can_close.clone();

        let close_btn = div()
            .id("window-close")
            .w(px(40.))
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
            .h(px(46.))
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
                    .pl(px(16.))
                    .pr(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .window_control_area(WindowControlArea::Drag)
                    .child(self.render_app_icon())
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(palette.text_primary)
                            .whitespace_nowrap()
                            .child(self.name.clone()),
                    )
                    .child(div().w(px(1.)).h(px(16.)).bg(palette.divider))
                    .child(self.render_version_pill(palette)),
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
                            .w(px(40.))
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
            (_, Some(f)) => (0.0_f32, f.max(0.03)),
            (_, None) => (
                indeterminate_offset(self.tick, INDETERMINATE_WIDTH),
                INDETERMINATE_WIDTH,
            ),
        };

        let bar = div()
            .flex_1()
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
            );

        div()
            .w_full()
            .max_w(px(520.))
            .flex()
            .items_center()
            .gap(px(12.))
            .child(bar)
            .when_some(fraction, |row, f| {
                let percent = (f * 100.0).round() as u8;
                row.child(
                    div()
                        .px(px(12.))
                        .py(px(4.))
                        .rounded_full()
                        .bg(palette.percent_pill_bg)
                        .border_1()
                        .border_color(palette.percent_pill_border)
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.percent_pill_text)
                        .whitespace_nowrap()
                        .flex_shrink_0()
                        .child(format!("{percent} %")),
                )
            })
    }

    fn render_stepper(&self, state: &UiState, palette: Palette) -> impl IntoElement {
        div()
            .w_full()
            .max_w(px(560.))
            .mt(px(4.))
            .flex()
            .items_start()
            .children(STEPS.iter().enumerate().map(|(index, label)| {
                let visual = state.step_visual(index);
                let left_done = index > 0 && state.step_visual(index - 1) == StepVisual::Completed;
                let right_done = index + 1 < STEPS.len() && visual == StepVisual::Completed;

                let left_line = div().flex_1().h(px(1.5)).when(index > 0, |line| {
                    line.bg(if left_done {
                        palette.step_line_done
                    } else {
                        palette.step_line_pending
                    })
                });

                let right_line = div()
                    .flex_1()
                    .h(px(1.5))
                    .when(index + 1 < STEPS.len(), |line| {
                        line.bg(if right_done {
                            palette.step_line_done
                        } else {
                            palette.step_line_pending
                        })
                    });

                let node = match visual {
                    StepVisual::Completed => div()
                        .size(px(24.))
                        .rounded_full()
                        .bg(palette.step_done_bg)
                        .text_color(palette.step_done_fg)
                        .text_size(px(12.))
                        .font_weight(FontWeight::BOLD)
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_shrink_0()
                        .child("✓"),
                    StepVisual::Active => div()
                        .size(px(24.))
                        .rounded_full()
                        .bg(palette.step_active_halo)
                        .border_2()
                        .border_color(palette.step_active_ring)
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_shrink_0()
                        .child(
                            div()
                                .size(px(10.))
                                .rounded_full()
                                .bg(palette.step_active_core),
                        ),
                    StepVisual::Restoring => div()
                        .size(px(24.))
                        .rounded_full()
                        .bg(palette.restore_start)
                        .text_color(rgb(0xffffff))
                        .text_size(px(12.))
                        .font_weight(FontWeight::BOLD)
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_shrink_0()
                        .child("↺"),
                    StepVisual::Failed => div()
                        .size(px(24.))
                        .rounded_full()
                        .bg(palette.error_start)
                        .text_color(rgb(0xffffff))
                        .text_size(px(12.))
                        .font_weight(FontWeight::BOLD)
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_shrink_0()
                        .child("✕"),
                    StepVisual::Pending => div()
                        .size(px(24.))
                        .rounded_full()
                        .border_2()
                        .border_color(palette.step_pending_border)
                        .flex_shrink_0(),
                };

                let (label_color, label_weight) = match visual {
                    StepVisual::Active | StepVisual::Restoring | StepVisual::Failed => {
                        (palette.text_primary, FontWeight::SEMIBOLD)
                    }
                    StepVisual::Completed => (palette.text_secondary, FontWeight::MEDIUM),
                    StepVisual::Pending => (palette.text_muted, FontWeight::NORMAL),
                };

                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child(left_line)
                            .child(node)
                            .child(right_line),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(label_weight)
                            .text_color(label_color)
                            .text_center()
                            .whitespace_nowrap()
                            .child(*label),
                    )
            }))
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
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(linear_gradient(
                145.,
                linear_color_stop(palette.bg_start, 0.),
                linear_color_stop(palette.bg_end, 1.),
            ))
            .text_color(palette.text_primary)
            .flex()
            .flex_col()
            // Subtle decorative ambient glows matching the reference design
            .child(
                div()
                    .absolute()
                    .top(px(-110.))
                    .right(px(-95.))
                    .size(px(260.))
                    .rounded_full()
                    .bg(palette.glow_bg)
                    .border_1()
                    .border_color(palette.glow_border),
            )
            .child(
                div()
                    .absolute()
                    .bottom(px(-140.))
                    .left(px(-100.))
                    .size(px(270.))
                    .rounded_full()
                    .bg(palette.glow_bg)
                    .border_1()
                    .border_color(palette.glow_border),
            )
            .child(self.render_titlebar(palette, closable))
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .px(px(36.))
                    .py(px(24.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(22.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(26.))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(title_color)
                                    .text_center()
                                    .child(state.headline()),
                            )
                            .child(
                                div()
                                    .max_w(px(500.))
                                    .text_size(px(13.5))
                                    .text_color(palette.text_secondary)
                                    .text_center()
                                    .child(state.event.message.clone()),
                            ),
                    )
                    .child(self.render_progress(&state, palette))
                    .child(self.render_stepper(&state, palette))
                    .when(state.event.terminal && closable, |content| {
                        content.child(
                            div()
                                .id("close-terminal-button")
                                .mt(px(4.))
                                .px(px(20.))
                                .py(px(7.))
                                .rounded_full()
                                .bg(palette.button_bg)
                                .border_1()
                                .border_color(palette.button_border)
                                .text_size(px(13.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(palette.text_primary)
                                .cursor_pointer()
                                .hover(move |s| s.bg(palette.button_hover))
                                .on_mouse_down(MouseButton::Left, |_, window, _| {
                                    window.remove_window();
                                })
                                .child("Cerrar"),
                        )
                    }),
            )
    }
}

fn window_options(title: &str, cx: &mut App) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(680.), px(390.)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(title.to_owned().into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        is_resizable: false,
        is_minimizable: true,
        ..Default::default()
    }
}

pub fn run(job: Job) -> anyhow::Result<bool> {
    let result = Arc::new(AtomicBool::new(false));
    let worker_result = result.clone();
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
            versions: VersionBadge::Transition {
                from: format!("v{}", job.current),
                to: format!("v{}", job.target),
            },
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
                cx.spawn(async move |entity, cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(60))
                            .await;
                        if entity
                            .update(cx, |view: &mut View, cx| {
                                view.tick = view.tick.wrapping_add(1);
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
                view
            })
        });
        let Ok(window) = window else {
            *window_failure.lock().expect("failure mutex") = window.err();
            cx.quit();
            return;
        };
        let worker = std::thread::spawn(move || {
            worker_result.store(
                job.run(|status| {
                    state.lock().expect("state mutex").apply_event(UpdateEvent {
                        stage: status.stage.clone(),
                        message: status.message.clone(),
                        terminal: status.terminal,
                        success: status.success,
                        progress: status.progress,
                    });
                }),
                Ordering::Release,
            );
            finished.store(true, Ordering::Release);
        });
        let _ = window.update(cx, |_, window, cx| {
            window.on_window_should_close(cx, move |_, _| can_close.load(Ordering::Acquire));
        });
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
                    versions: VersionBadge::Note("No se pudo preparar la actualización".into()),
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
    fn maps_all_veltrix_stages_to_five_steps() {
        let mut state = UiState::new(event("Preparando actualización", "Inicio", false, false));
        assert_eq!(state.active_step, 0);
        assert_eq!(state.phase_kind(), PhaseKind::Running);
        assert_eq!(state.headline(), "Aplicando actualización…");
        assert_eq!(state.step_visual(0), StepVisual::Active);
        assert_eq!(state.step_visual(1), StepVisual::Pending);

        state.apply_event(event(
            "Verificando integridad",
            "Copiando el instalador.",
            false,
            false,
        ));
        assert_eq!(state.active_step, 0);

        state.apply_event(event(
            "Esperando cierre de la aplicación",
            "Cierra la aplicación.",
            false,
            false,
        ));
        assert_eq!(state.active_step, 1);
        assert_eq!(state.step_visual(0), StepVisual::Completed);
        assert_eq!(state.step_visual(1), StepVisual::Active);

        state.apply_event(event(
            "Instalando actualización",
            "Ejecutando instalador.",
            false,
            false,
        ));
        assert_eq!(state.active_step, 2);
        assert_eq!(state.step_visual(1), StepVisual::Completed);
        assert_eq!(state.step_visual(2), StepVisual::Active);

        state.apply_event(event("Validando instalación", "Validando.", false, false));
        assert_eq!(state.active_step, 3);
        assert_eq!(state.step_visual(2), StepVisual::Completed);
        assert_eq!(state.step_visual(3), StepVisual::Active);

        state.apply_event(event(
            "Reiniciando aplicación",
            "Reiniciando.",
            false,
            false,
        ));
        assert_eq!(state.active_step, 4);
        assert_eq!(state.step_visual(3), StepVisual::Completed);
        assert_eq!(state.step_visual(4), StepVisual::Active);

        state.apply_event(event("Actualización completada", "Listo.", true, true));
        assert_eq!(state.phase_kind(), PhaseKind::Succeeded);
        for idx in 0..STEPS.len() {
            assert_eq!(state.step_visual(idx), StepVisual::Completed);
        }
    }

    #[test]
    fn preserves_failed_step_during_restoration_and_terminal_failure() {
        let mut state = UiState::new(event("Instalando actualización", "NSIS", false, false));
        assert_eq!(state.active_step, 2);

        state.apply_event(event(
            "Restaurando instalación",
            "Aplicando respaldo.",
            false,
            false,
        ));
        assert_eq!(state.active_step, 2);
        assert_eq!(state.phase_kind(), PhaseKind::Restoring);
        assert_eq!(state.step_visual(0), StepVisual::Completed);
        assert_eq!(state.step_visual(1), StepVisual::Completed);
        assert_eq!(state.step_visual(2), StepVisual::Restoring);
        assert_eq!(state.step_visual(3), StepVisual::Pending);

        state.apply_event(event(
            "Actualización fallida",
            "NSIS falló. Se restauró la versión 1.0.0.",
            true,
            false,
        ));
        assert_eq!(state.phase_kind(), PhaseKind::RestoredAfterFailure);
        assert_eq!(state.step_visual(2), StepVisual::Restoring);

        state.apply_event(event("Actualización fallida", "Error fatal.", true, false));
        assert_eq!(state.phase_kind(), PhaseKind::Failed);
        assert_eq!(state.step_visual(2), StepVisual::Failed);
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

        assert_eq!(dark.bg_start, vibrant_dark.bg_start);
        assert_eq!(light.bg_start, vibrant_light.bg_start);
        assert_ne!(dark.bg_start, light.bg_start);
    }
}
