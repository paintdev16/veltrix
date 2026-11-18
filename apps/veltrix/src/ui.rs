use crate::windows::Job;
use gpui_kit::{
    App, Bounds, Context, Render, Window, WindowBounds, WindowOptions, div, img, prelude::*, px,
    relative, rgb, size,
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

struct View {
    name: String,
    versions: String,
    icon: Option<PathBuf>,
    event: Arc<Mutex<UpdateEvent>>,
    tick: usize,
}

impl Render for View {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let event = self.event.lock().expect("event mutex");
        let accent = if event.terminal && !event.success {
            0xff867e
        } else {
            0x76e6b2
        };
        let fraction = event
            .progress
            .map(|(done, total)| done as f32 / total as f32);
        let icon = div()
            .size(px(48.))
            .rounded_lg()
            .bg(rgb(0x203737))
            .flex()
            .justify_center()
            .items_center()
            .text_color(rgb(accent))
            .child(match &self.icon {
                Some(path) => img(path.clone()).size(px(36.)).into_any_element(),
                None => div().text_xl().child("↑").into_any_element(),
            });
        div()
            .size_full()
            .bg(rgb(0x141b23))
            .text_color(rgb(0xf0f4f8))
            .p_7()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div().flex().items_center().gap_4().child(icon).child(
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_xl().child(self.name.clone()))
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(0xaab8c7))
                                .child(self.versions.clone()),
                        ),
                ),
            )
            .child(
                div()
                    .rounded_lg()
                    .bg(rgb(0x202a35))
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .text_color(rgb(accent))
                            .child(event.stage.clone()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xc5d0dc))
                            .child(event.message.clone()),
                    )
                    .when_some(fraction, |panel, fraction| {
                        panel.child(div().text_sm().child(format!("{:.1} %", fraction * 100.)))
                    })
                    .when(!event.terminal, |panel| {
                        panel.child(
                            div()
                                .w_full()
                                .h(px(5.))
                                .bg(rgb(0x384655))
                                .rounded_full()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .ml(relative(if fraction.is_some() {
                                            0.
                                        } else {
                                            (self.tick % 21) as f32 * 0.04
                                        }))
                                        .w(relative(fraction.unwrap_or(0.20)))
                                        .h_full()
                                        .bg(rgb(accent))
                                        .rounded_full(),
                                ),
                        )
                    }),
            )
    }
}

pub fn run(job: Job) -> anyhow::Result<bool> {
    let result = Arc::new(AtomicBool::new(false));
    let worker_result = result.clone();
    let failure = Arc::new(Mutex::new(None));
    let window_failure = failure.clone();
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        let event = Arc::new(Mutex::new(UpdateEvent {
            stage: "Preparando actualización".into(),
            message: "Comprobando el paquete y la configuración local.".into(),
            terminal: false,
            success: false,
            progress: None,
        }));
        let view = View {
            name: job.name.clone(),
            versions: format!("{} → {}", job.current, job.target),
            icon: job.icon.clone(),
            event: event.clone(),
            tick: 0,
        };
        let bounds = Bounds::centered(None, size(px(570.), px(315.)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                is_resizable: false,
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("Veltrix");
                cx.new(|cx| {
                    cx.spawn(async move |entity, cx| {
                        loop {
                            cx.background_executor()
                                .timer(Duration::from_millis(70))
                                .await;
                            if entity
                                .update(cx, |view: &mut View, cx| {
                                    view.tick += 1;
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
            },
        );
        let Ok(_) = window else {
            *window_failure.lock().expect("failure mutex") = window.err();
            cx.quit();
            return;
        };
        let finished = Arc::new(AtomicBool::new(false));
        let can_close = finished.clone();
        let worker = std::thread::spawn(move || {
            worker_result.store(
                job.run(|status| {
                    *event.lock().expect("event mutex") = UpdateEvent {
                        stage: status.stage.clone(),
                        message: status.message.clone(),
                        terminal: status.terminal,
                        success: status.success,
                        progress: status.progress,
                    };
                }),
                Ordering::Release,
            );
            finished.store(true, Ordering::Release);
        });
        let _ = window.unwrap().update(cx, |_, window, cx| {
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
        let bounds = Bounds::centered(None, size(px(570.), px(315.)), cx);
        let event = Arc::new(Mutex::new(UpdateEvent {
            stage: "Actualización fallida".into(),
            message,
            terminal: true,
            success: false,
            progress: None,
        }));
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                is_resizable: false,
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("Veltrix");
                cx.new(|_| View {
                    name: "Veltrix".into(),
                    versions: "No se pudo preparar la actualización".into(),
                    icon: None,
                    event,
                    tick: 0,
                })
            },
        );
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
