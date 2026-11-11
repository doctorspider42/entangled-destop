//! The machine editor: a drawer from the right edge with the sections down a
//! side list, each a few titled groups of settings (`ui::rows`).
//!
//! The shape follows one rule: the common choice is one click, and the rare
//! one is still there but never the main course. A screen size is "follow the
//! window" or a named size, with "Custom…" at the end of the list; a refresh
//! rate is "match monitor" or a standard rate; paths and engine plumbing live
//! under each section's **Advanced**.

use egui::{Align, Layout, RichText, Vec2};

use super::dialogs::{entry_label, option_card, POWER_PRESETS};
use super::rows;
use super::{self as ui, Icon};
use crate::app::{Action, EditVmSection};
use crate::backend::Backend;
use crate::editor::{EditForm, NetworkChoice};
use crate::picker::PickTarget;
use crate::{launcher, theme, wslengine};
use control_api::GpuRenderer;

/// The screen sizes offered by name. Anything else is "Custom".
const SCREEN_SIZES: [(u32, u32, &str); 5] = [
    (1280, 720, "HD"),
    (1600, 900, ""),
    (1920, 1080, "Full HD"),
    (2560, 1440, "QHD"),
    (3840, 2160, "4K"),
];

/// The refresh rates offered by name, beside "Match monitor".
const REFRESH_RATES: [u32; 4] = [60, 75, 120, 144];

/// The machine editor as a drawer from the right edge: the machine's page
/// stays visible behind it, dimmed, so the change being made sits beside the
/// thing it changes. One section at a time, chosen from the list on the left
/// (or by the setup tile that opened it); Save writes them all.
pub(super) fn edit_vm_drawer(
    ctx: &egui::Context,
    state: &mut crate::app::EditVmState,
    wsl_engine: &wslengine::Status,
    actions: &mut Vec<Action>,
) -> bool {
    let screen = ctx.screen_rect();
    let width = 880.0_f32.min(screen.width() - 40.0);
    let id = egui::Id::new("edit-vm");
    let area = egui::Modal::default_area(id).anchor(egui::Align2::RIGHT_TOP, Vec2::ZERO);
    let response = egui::Modal::new(id)
        .area(area)
        .backdrop_color(egui::Color32::from_black_alpha(150))
        .frame(
            egui::Frame::new()
                .fill(theme::MODAL)
                .stroke(egui::Stroke::new(1.0_f32, theme::STROKE_STRONG))
                .shadow(egui::Shadow {
                    offset: [-24, 0],
                    blur: 60,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(110),
                }),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.set_height(screen.height());
            let sections = [
                (EditVmSection::Hardware, Icon::Chip, "Processor & memory"),
                (EditVmSection::DisplayGpu, Icon::Screen, "Display"),
                (
                    EditVmSection::NetworkDevices,
                    Icon::Globe,
                    "Network & devices",
                ),
                (EditVmSection::Storage, Icon::Disk, "Storage"),
                (EditVmSection::BootMedia, Icon::Power, "Startup"),
            ];

            // Header: whose settings, and a way out.
            egui::Frame::new()
                .inner_margin(egui::Margin {
                    left: 24,
                    right: 16,
                    top: 16,
                    bottom: 16,
                })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (rect, _) =
                            ui.allocate_exact_size(Vec2::splat(36.0), egui::Sense::hover());
                        ui.painter().rect_filled(
                            rect,
                            egui::CornerRadius::same(9),
                            theme::SELECTED,
                        );
                        ui.painter().rect_stroke(
                            rect,
                            egui::CornerRadius::same(9),
                            egui::Stroke::new(1.0_f32, theme::STROKE_STRONG),
                            egui::StrokeKind::Inside,
                        );
                        ui::paint_icon(
                            ui.painter(),
                            egui::Rect::from_center_size(rect.center(), Vec2::splat(18.0)),
                            Icon::Screen,
                            theme::ACCENT,
                        );
                        ui.add_space(6.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(
                                RichText::new(&state.form.name)
                                    .size(16.0)
                                    .color(theme::TEXT)
                                    .strong(),
                            );
                            ui.label(ui::dim("Machine settings"));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui::icon_button(ui, Icon::Close, "Close without saving").clicked() {
                                actions.push(Action::CloseModal);
                            }
                        });
                    });
                });
            theme::rule(ui.painter(), ui.max_rect().x_range(), ui.cursor().top());

            let preview = state.form.to_config();
            let dirty = state.form.dirty();
            let savable = preview.is_ok() && dirty;
            egui::TopBottomPanel::bottom("edit-vm-footer")
                .frame(
                    egui::Frame::new()
                        .fill(theme::MODAL)
                        .inner_margin(egui::Margin::symmetric(24, 14)),
                )
                .show_inside(ui, |ui| {
                    if let Err(message) = &preview {
                        ui.label(RichText::new(message).color(theme::ERR).size(12.5));
                        ui.add_space(8.0);
                    } else if let Some(error) = &state.error {
                        ui.label(RichText::new(error).color(theme::ERR).size(12.5));
                        ui.add_space(8.0);
                    }
                    ui.horizontal(|ui| {
                        if dirty {
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
                            ui.painter()
                                .circle_filled(rect.center(), 3.5, theme::ACCENT);
                            ui.label(ui::dim(format!(
                                "Unsaved changes · they apply the next time {} starts",
                                state.form.name
                            )));
                        } else {
                            ui.label(ui::faint(format!(
                                "Changes apply the next time {} starts",
                                state.form.name
                            )));
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let save = ui::primary_button_enabled(ui, "Save", savable)
                                .on_hover_text("Validates and saves every section");
                            if save.clicked() && savable {
                                actions.push(Action::SubmitEditVm);
                            }
                            if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
                                actions.push(Action::CloseModal);
                            }
                        });
                    });
                });

            egui::SidePanel::left("edit-vm-nav")
                .exact_width(208.0)
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(theme::BG_PANEL)
                        .inner_margin(egui::Margin::symmetric(10, 14)),
                )
                .show_inside(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for (section, icon, label) in sections {
                        if rows::nav_item(ui, icon, label, state.section == section).clicked() {
                            state.section = section;
                        }
                    }
                });

            egui::CentralPanel::default()
                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(28, 22)))
                .show_inside(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            // Clear of the scroll bar, so groups never touch it.
                            ui.set_width(ui.available_width() - 12.0);
                            ui.spacing_mut().item_spacing.y = 8.0;
                            let devices = preview.as_ref().ok().map(|cfg| {
                                (
                                    cfg.virtio_devices(),
                                    control_api::max_devices(cfg.transport),
                                )
                            });
                            match state.section {
                                EditVmSection::Hardware => {
                                    edit_hardware(ui, &mut state.form, wsl_engine)
                                }
                                EditVmSection::BootMedia => {
                                    edit_startup(ui, &mut state.form, actions);
                                }
                                EditVmSection::DisplayGpu => edit_display(ui, &mut state.form),
                                EditVmSection::NetworkDevices => {
                                    edit_network_devices(ui, &mut state.form, devices.as_ref());
                                }
                                EditVmSection::Storage => {
                                    edit_storage(ui, &mut state.form, actions)
                                }
                            }
                            ui.add_space(12.0);
                        });
                });
        });
    response.should_close()
}

/// Space between one section's group and the next title.
fn gap(ui: &mut egui::Ui) {
    ui.add_space(14.0);
}

/// "8 GB", "1.5 GB" — memory as people say it.
fn gb(mib: u64) -> String {
    if mib % 1024 == 0 {
        format!("{} GB", mib / 1024)
    } else {
        format!("{:.1} GB", mib as f64 / 1024.0)
    }
}

/// A combo box sized for the right-hand side of a row.
fn combo(id: &str, selected: impl Into<egui::WidgetText>) -> egui::ComboBox {
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected)
        .width(220.0)
        .height(320.0)
}

/// A flag kept for one editor session, outside the form: "the person picked
/// Custom", which the values alone cannot say when they match a preset.
fn flag(ui: &egui::Ui, key: &str) -> bool {
    ui.data(|d| d.get_temp(egui::Id::new(("edit-vm-flag", key))))
        .unwrap_or(false)
}

fn set_flag(ui: &egui::Ui, key: &str, value: bool) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new(("edit-vm-flag", key)), value));
}

// ---------------------------------------------------------------------------
// Processor & memory
// ---------------------------------------------------------------------------

fn edit_hardware(ui: &mut egui::Ui, form: &mut EditForm, wsl_engine: &wslengine::Status) {
    use control_api::VirtioTransport;

    rows::section_title(ui, "Performance");
    let preset = POWER_PRESETS
        .iter()
        .position(|p| p.vcpus == form.vcpus && p.memory_mib == form.memory_mib);
    let power_key = format!("custom-power:{}", form.name);
    let custom = flag(ui, &power_key) || preset.is_none();
    ui.horizontal(|ui| {
        let gap = ui.spacing().item_spacing.x;
        let card_w = ((ui.available_width() - 3.0 * gap) / 4.0).floor();
        for (index, p) in POWER_PRESETS.iter().enumerate() {
            let detail = format!("{} cores · {}\nFor {}", p.vcpus, gb(p.memory_mib), p.hint);
            let selected = !custom && preset == Some(index);
            if option_card(ui, card_w, p.name, &detail, selected, true, theme::ACCENT).clicked() {
                form.vcpus = p.vcpus;
                form.memory_mib = p.memory_mib;
                set_flag(ui, &power_key, false);
            }
        }
        if option_card(
            ui,
            card_w,
            "Custom",
            "Pick your own\ncores and memory",
            custom,
            true,
            theme::ACCENT,
        )
        .clicked()
        {
            set_flag(ui, &power_key, true);
        }
    });

    let host_cores = form.host_cpus.max(1);
    rows::group(ui, |g| {
        if custom {
            g.row(
                "Processor cores",
                "More than this computer has only makes the guest slower.",
                "How many processor cores the guest sees. More than this computer physically \
                 has only makes the guest slower.",
                |ui| {
                    ui.label(
                        RichText::new(format!("{} cores", form.vcpus))
                            .size(13.5)
                            .color(theme::TEXT),
                    );
                    ui.spacing_mut().slider_width = 220.0;
                    ui.add(egui::Slider::new(&mut form.vcpus, 1..=64).show_value(false));
                },
            );
            g.row(
                "Memory",
                "Taken from this computer only while the machine runs.",
                "How much of this computer's RAM the machine may use while it runs. It is \
                 taken from the host only while the machine is on.",
                |ui| {
                    ui.label(
                        RichText::new(gb(form.memory_mib))
                            .size(13.5)
                            .color(theme::TEXT),
                    );
                    ui.spacing_mut().slider_width = 220.0;
                    ui.add(
                        egui::Slider::new(
                            &mut form.memory_mib,
                            control_api::MIN_MEMORY_MIB..=control_api::MAX_MEMORY_MIB,
                        )
                        .step_by(128.0)
                        .show_value(false),
                    );
                },
            );
        }
        g.body(|ui| {
            ui.columns(2, |columns| {
                rows::usage(
                    &mut columns[0],
                    "Processor",
                    &format!("{} of {host_cores} cores", form.vcpus),
                    form.vcpus as f32 / host_cores as f32,
                );
                match form.host_memory_mib {
                    Some(total) if total > 0 => rows::usage(
                        &mut columns[1],
                        "Memory",
                        &format!("{} of {}", gb(form.memory_mib), gb(total)),
                        form.memory_mib as f32 / total as f32,
                    ),
                    _ => rows::usage(&mut columns[1], "Memory", &gb(form.memory_mib), 0.0),
                }
            });
        });
    });
    ui.label(ui::faint(format!(
        "Taken from this computer only while {} is running.",
        form.name
    )));

    gap(ui);
    rows::section_title(ui, "Memory");
    rows::group(ui, |g| {
        g.row(
            "Give unused memory back",
            "When the guest frees memory, this computer gets it back while it runs.",
            "Without it, memory the guest has used stays taken on this computer until the \
             machine stops, even after the guest frees it. With it, Linux guests report free \
             memory (in 2 MiB blocks, a moment after it is freed) and Entangled hands it back. \
             Costs one of the machine's device slots.",
            |ui| {
                rows::switch(ui, &mut form.reclaim_memory, "Give unused memory back");
            },
        );
    });

    gap(ui);
    let transport_label = |t: VirtioTransport| match t {
        VirtioTransport::Pci => "PCI (like a PC)",
        VirtioTransport::Mmio => "MMIO (minimal)",
    };
    let choices: Vec<Backend> = Backend::ALL
        .into_iter()
        .filter(|backend| backend.available_on_host())
        .collect();
    let summary = format!(
        "{} engine · {}",
        form.backend.label(),
        transport_label(form.transport)
    );
    if rows::disclosure(ui, "hardware-advanced", "Advanced", &summary) {
        rows::group(ui, |g| {
            if choices.len() > 1 {
                g.row(
                    "Engine",
                    "Windows runs everything. WSL adds OpenGL 3D and bridged networking.",
                    "Where machines run. Windows is the usual choice. WSL (the Windows \
                     Subsystem for Linux) adds OpenGL 3D and bridged networking, after a \
                     one-time setup.",
                    |ui| {
                        combo("edit-backend", entry_label(form.backend, wsl_engine)).show_ui(
                            ui,
                            |ui| {
                                for backend in &choices {
                                    ui.selectable_value(
                                        &mut form.backend,
                                        *backend,
                                        entry_label(*backend, wsl_engine),
                                    )
                                    .on_hover_text(backend.tooltip());
                                }
                            },
                        );
                    },
                );
            }
            g.row(
                "Device wiring",
                "PCI looks like a normal PC. Leave it unless you need MMIO.",
                "How the guest sees its virtual disks, network and screen. PCI is what an \
                 ordinary PC looks like and is the right answer unless you know otherwise; \
                 MMIO is a leaner wiring used by minimal direct-boot setups.",
                |ui| {
                    combo("edit-transport", transport_label(form.transport)).show_ui(ui, |ui| {
                        for t in [VirtioTransport::Pci, VirtioTransport::Mmio] {
                            ui.selectable_value(&mut form.transport, t, transport_label(t));
                        }
                    });
                },
            );
        });
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

fn edit_display(ui: &mut egui::Ui, form: &mut EditForm) {
    rows::section_title(ui, "Screen");
    rows::group(ui, |g| {
        let detail = if form.follow_window {
            format!(
                "Resize or maximise the window and the desktop follows, pixel for pixel. \
                 Starts at {} × {}.",
                form.display_width, form.display_height
            )
        } else {
            "The desktop keeps this size; the window scales the picture.".to_string()
        };
        g.row(
            "Resolution",
            &detail,
            "Follow window: resize, maximise or go fullscreen and the guest's desktop becomes \
             exactly the window's size. Fixed size: the guest keeps the size you pick, \
             whatever the window does, and the picture is scaled instead.",
            |ui| {
                rows::segmented(
                    ui,
                    &mut form.follow_window,
                    &[(true, "Follow window"), (false, "Fixed size")],
                );
            },
        );
        if !form.follow_window {
            g.sub_row(
                "Size",
                "",
                "The size of the guest's virtual screen, in pixels.",
                |ui| screen_size_picker(ui, form),
            );
        }
        g.row(
            "Refresh rate",
            "Higher feels smoother. Match monitor follows this display.",
            "How often the virtual monitor says it refreshes. The guest's desktop draws in \
             step with it, so a frame that misses one refresh waits for the next: a higher \
             rate makes animation smoother and lowers latency. \"Match monitor\" picks this \
             computer's monitor rate — or, above 144 Hz, the largest even fraction of it — \
             the same rule a new machine gets.",
            |ui| refresh_picker(ui, form),
        );
    });

    gap(ui);
    rows::section_title(ui, "Mouse & keyboard");
    rows::group(ui, |g| {
        let detail = if form.gaming_mouse {
            "Captures the mouse for free camera movement in games. Ctrl+Alt lets go."
        } else {
            "The guest's pointer follows your mouse exactly."
        };
        g.row(
            "Mouse",
            detail,
            "Desktop keeps the guest pointer aligned with your mouse. Games uses relative \
             movement for unlimited camera rotation. Ctrl+Alt releases the mouse.",
            |ui| {
                rows::segmented(
                    ui,
                    &mut form.gaming_mouse,
                    &[(false, "Desktop"), (true, "Games")],
                );
            },
        );
        g.row(
            "Keyboard layout",
            "Used when you paste text into the guest with Ctrl+Alt+V.",
            "The keyboard layout set inside the guest, so \"type the clipboard\" \
             (Ctrl+Alt+V) can type its letters. Polish (programmers) adds ą ć ę ł ń ó ś ź ż \
             as AltGr + the letter.",
            |ui| {
                let label = match form.typing_layout.as_str() {
                    "pl" => "Polish (programmers)",
                    _ => "English (US)",
                };
                combo("typing-layout", label).show_ui(ui, |ui| {
                    ui.selectable_value(&mut form.typing_layout, String::new(), "English (US)");
                    ui.selectable_value(
                        &mut form.typing_layout,
                        "pl".to_string(),
                        "Polish (programmers)",
                    );
                });
            },
        );
    });

    gap(ui);
    rows::section_title(ui, "3D graphics");
    if form.gpu == GpuRenderer::Virgl && form.backend.gpu_block(GpuRenderer::Virgl).is_some() {
        // Unavailable here — and never silently left on, because the engine
        // refuses to boot a profile that asks for 3D it cannot deliver.
        form.gpu = GpuRenderer::TwoD;
    }
    rows::group(ui, |g| {
        for choice in [GpuRenderer::Venus, GpuRenderer::Virgl, GpuRenderer::TwoD] {
            let block = form.backend.gpu_block(choice);
            let (title, detail) = gpu_choice(choice);
            let response = g.choice(
                title,
                block.map_or(detail, |b| b.short),
                form.gpu == choice,
                block.is_none(),
            );
            let response = match block {
                Some(reason) => response.on_hover_text(reason.long),
                None => response.on_hover_text(gpu_hint(choice)),
            };
            if response.clicked() {
                form.gpu = choice;
            }
        }
    });
    if form.gpu != GpuRenderer::Venus {
        return;
    }
    rows::callout(
        ui,
        "The guest needs Entangled's graphics drivers. Machines installed with 3D already \
         have them; for any other, use Add 3D graphics… in the machine's … menu.",
    );
    rows::group(ui, |g| {
        g.row(
            "Performance boost",
            "Full graphics-card clocks while the guest draws. NVIDIA only.",
            "Asks the graphics driver to run the card at full clocks while the guest is \
             drawing, and lets it settle a second after it stops. Makes the guest's 3D \
             noticeably faster on a 60 Hz desktop and costs nothing while it is idle; turn \
             it off to save power. NVIDIA only — other cards ignore it.",
            |ui| {
                rows::switch(ui, &mut form.gpu_boost, "Performance boost");
            },
        );
        g.row(
            "Video memory",
            "How much of the card the guest may take. Your own desktop keeps the rest.",
            "The most of this computer's video memory the guest's programs may take \
             together. Automatic gives them three quarters of it and your own desktop keeps \
             the rest; a limit protects the host from a guest that runs away with memory. \
             Never more than the card has, whatever it says.",
            |ui| {
                let label = if form.gpu_memory_limit {
                    "A limit I set"
                } else {
                    "Automatic · up to ¾ of the card"
                };
                combo("gpu-memory", label).show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut form.gpu_memory_limit,
                        false,
                        "Automatic · up to ¾ of the card",
                    );
                    ui.selectable_value(&mut form.gpu_memory_limit, true, "A limit I set");
                });
            },
        );
        if form.gpu_memory_limit {
            g.sub_row("Limit", "", "", |ui| {
                ui.label(
                    RichText::new(gb(u64::from(form.gpu_memory_mib)))
                        .size(13.5)
                        .color(theme::TEXT),
                );
                // Up to 24 GB, the largest cards sold; a bigger cap a profile
                // already holds widens the range rather than being clamped.
                let top = form.gpu_memory_mib.max(24 * 1024);
                ui.spacing_mut().slider_width = 240.0;
                ui.add(
                    egui::Slider::new(
                        &mut form.gpu_memory_mib,
                        control_api::MIN_GPU_MEMORY_MIB..=top,
                    )
                    .step_by(256.0)
                    .show_value(false),
                );
            });
        }
    });
}

/// The fixed-size picker: a named size, or "Custom…" and two numbers.
fn screen_size_picker(ui: &mut egui::Ui, form: &mut EditForm) {
    let key = format!("custom-size:{}", form.name);
    let named = SCREEN_SIZES
        .iter()
        .find(|(w, h, _)| *w == form.display_width && *h == form.display_height);
    let custom = flag(ui, &key) || named.is_none();
    let label = |(w, h, name): (u32, u32, &str)| {
        if name.is_empty() {
            format!("{w} × {h}")
        } else {
            format!("{w} × {h} · {name}")
        }
    };
    let selected = match named {
        Some(size) if !custom => label(*size),
        _ => "Custom".to_string(),
    };
    combo("screen-size", selected).show_ui(ui, |ui| {
        for size in SCREEN_SIZES {
            let on = !custom && named == Some(&size);
            if ui.selectable_label(on, label(size)).clicked() {
                form.display_width = size.0;
                form.display_height = size.1;
                set_flag(ui, &key, false);
            }
        }
        if ui.selectable_label(custom, "Custom…").clicked() {
            set_flag(ui, &key, true);
        }
    });
    if custom {
        ui.add(egui::DragValue::new(&mut form.display_height).range(200..=4320));
        ui.label(ui::faint("×"));
        ui.add(egui::DragValue::new(&mut form.display_width).range(320..=7680));
    }
}

/// "Match monitor", the standard rates, or "Custom…" and a number.
fn refresh_picker(ui: &mut egui::Ui, form: &mut EditForm) {
    let key = format!("custom-refresh:{}", form.name);
    let monitor = form.monitor_refresh_hz;
    let rates: Vec<u32> = REFRESH_RATES
        .into_iter()
        .filter(|hz| (control_api::MIN_REFRESH_HZ..=control_api::MAX_REFRESH_HZ).contains(hz))
        .collect();
    let custom =
        flag(ui, &key) || (form.refresh_hz != monitor && !rates.contains(&form.refresh_hz));
    let match_label = format!("Match monitor · {monitor} Hz");
    let selected = if custom {
        "Custom".to_string()
    } else if form.refresh_hz == monitor {
        match_label.clone()
    } else {
        format!("{} Hz", form.refresh_hz)
    };
    combo("refresh-rate", selected)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(!custom && form.refresh_hz == monitor, match_label.as_str())
                .clicked()
            {
                form.refresh_hz = monitor;
                set_flag(ui, &key, false);
            }
            for hz in &rates {
                let on = !custom && form.refresh_hz == *hz && *hz != monitor;
                if ui.selectable_label(on, format!("{hz} Hz")).clicked() {
                    form.refresh_hz = *hz;
                    set_flag(ui, &key, false);
                }
            }
            if ui.selectable_label(custom, "Custom…").clicked() {
                set_flag(ui, &key, true);
            }
        })
        .response
        .on_hover_text(
            "This computer's primary monitor, snapped to a standard rate and held to what \
             the GPU desktop keeps up with.",
        );
    if custom {
        ui.label(ui::faint("Hz"));
        ui.add(
            egui::DragValue::new(&mut form.refresh_hz)
                .range(control_api::MIN_REFRESH_HZ..=control_api::MAX_REFRESH_HZ),
        );
    }
}

/// The 3D choice in the words the list uses.
fn gpu_choice(renderer: GpuRenderer) -> (&'static str, &'static str) {
    match renderer {
        GpuRenderer::Venus => (
            "Use this computer's graphics card",
            "Fast Vulkan and OpenGL inside the guest.",
        ),
        GpuRenderer::Virgl => ("OpenGL only (virgl)", "OpenGL through the WSL engine."),
        GpuRenderer::TwoD => (
            "Off",
            "The guest draws its desktop on its own processor. Works everywhere.",
        ),
    }
}

fn gpu_hint(renderer: GpuRenderer) -> &'static str {
    match renderer {
        GpuRenderer::TwoD => {
            "No 3D. The guest draws its desktop with its own processor, which works \
             everywhere."
        }
        GpuRenderer::Virgl => "OpenGL through the host's virglrenderer. Linux/KVM backends only.",
        GpuRenderer::Venus => {
            "The guest's Vulkan, and its OpenGL and desktop through Zink, on this \
             computer's graphics card. Either backend; needs a Vulkan driver, which \
             Diagnostics checks. If the host cannot provide it, the machine refuses to \
             start rather than quietly falling back to software rendering."
        }
    }
}

// ---------------------------------------------------------------------------
// Network & devices
// ---------------------------------------------------------------------------

/// A port forward being typed, before it becomes a rule.
#[derive(Debug, Clone, Default)]
struct ForwardDraft {
    open: bool,
    udp: bool,
    lan: bool,
    host: String,
    guest: String,
}

impl ForwardDraft {
    /// The rule in the profile's own syntax, when both ports are numbers.
    fn rule(&self) -> Option<String> {
        let text = format!(
            "{}{}{}:{}",
            if self.udp { "udp:" } else { "" },
            if self.lan { "0.0.0.0:" } else { "" },
            self.host.trim(),
            self.guest.trim()
        );
        control_api::PortForwardRule::parse(&text)
            .ok()
            .map(|_| text)
    }
}

fn edit_network_devices(
    ui: &mut egui::Ui,
    form: &mut EditForm,
    devices: Option<&(Vec<&'static str>, usize)>,
) {
    rows::section_title(ui, "Network");
    let tap_block = form.backend.tap_block();
    ui.horizontal(|ui| {
        let gap = ui.spacing().item_spacing.x;
        let card_w = ((ui.available_width() - 2.0 * gap) / 3.0).floor();
        for (choice, title, detail) in [
            (
                NetworkChoice::Usernet,
                "Internet",
                "Private, behind this\ncomputer. No setup.",
            ),
            (
                NetworkChoice::Tap,
                "On my network",
                "Its own address on\nyour network.",
            ),
            (NetworkChoice::None, "Offline", "No network card\nat all."),
        ] {
            let blocked = choice == NetworkChoice::Tap && tap_block.is_some();
            let response = option_card(
                ui,
                card_w,
                title,
                detail,
                form.network == choice,
                !blocked,
                theme::ACCENT,
            );
            let response = match tap_block.filter(|_| blocked) {
                Some(reason) => response.on_hover_text(reason.long),
                None => response,
            };
            if response.clicked() {
                form.network = choice;
            }
        }
    });
    // A profile carried over from a Linux host can still *say* tap; the reason
    // has to be visible, not just the card greyed out.
    if form.network == NetworkChoice::Tap {
        if let Some(reason) = tap_block {
            ui.label(RichText::new(reason.short).color(theme::WARN).size(12.0));
        }
        rows::group(ui, |g| {
            g.row(
                "Host interface",
                "The network interface on this computer to attach to.",
                "The name of the host network interface to attach to, created once by \
                 scripts/setup-tap.sh.",
                |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut form.interface)
                            .desired_width(200.0)
                            .hint_text("entangled0"),
                    );
                },
            );
        });
    }
    if form.network == NetworkChoice::Usernet {
        port_forwards(ui, form);
    }

    gap(ui);
    rows::section_title(ui, "Devices");
    rows::group(ui, |g| {
        g.row(
            "Sound",
            "Plays the guest's audio through this computer's speakers.",
            "Gives the machine a sound card, so the guest can play audio out through this \
             computer's speakers. Every current Linux desktop and Windows has the driver.",
            |ui| {
                rows::switch(ui, &mut form.sound, "Sound");
            },
        );
        g.row(
            "Gamepads",
            "Passes the controllers plugged into this computer through to the guest.",
            "Xbox-shaped controllers the guest recognises with no drivers from us. \
             Entangled reads whichever pads this computer has — XInput on Windows, evdev \
             on Linux — and the guest sees them even with nothing plugged in yet, so a pad \
             can be connected while the machine is running.",
            |ui| {
                rows::switch(ui, &mut form.gamepad, "Gamepads");
            },
        );
        if form.gamepad {
            g.sub_row(
                "Players",
                "",
                "How many controllers the guest sees. Each one is a separate device inside \
                 the machine. Player 1 is the pad the guest calls js0.",
                |ui| {
                    rows::stepper(
                        ui,
                        &mut form.gamepad_players,
                        1..=control_api::MAX_GAMEPAD_PLAYERS,
                        "players",
                    );
                },
            );
        }
        if let Some((used, max)) = devices {
            g.body(|ui| device_slots(ui, used, *max));
        }
    });

    gap(ui);
    let mac = if form.mac.trim().is_empty() {
        "MAC automatic".to_string()
    } else {
        format!("MAC {}", form.mac.trim())
    };
    let summary = if form.sound {
        format!("{mac} · audio {}", form.sound_backend)
    } else {
        mac
    };
    if rows::disclosure(ui, "network-advanced", "Advanced", &summary) {
        rows::group(ui, |g| {
            if form.network != NetworkChoice::None {
                g.row(
                    "MAC address",
                    "Leave empty for a stable one made from the machine's name.",
                    "The hardware address the guest sees. Leave it empty and Entangled \
                     derives a stable one from the machine's name.",
                    |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut form.mac)
                                .desired_width(200.0)
                                .hint_text("automatic"),
                        );
                    },
                );
            }
            if form.sound {
                // Only this engine's own backend is offered: `alsa` on a
                // Windows engine (or `wasapi` on a Linux one) is not a degraded
                // mode, it is a machine that refuses to start.
                g.row(
                    "Audio output",
                    "Automatic uses whatever this computer has.",
                    "Which of this computer's audio systems the card plays into. Leave it \
                     on \"auto\" unless you have a reason: auto uses whatever the host has \
                     and never prevents a machine from starting.",
                    |ui| {
                        combo("edit-sound-backend", form.sound_backend.to_string()).show_ui(
                            ui,
                            |ui| {
                                for choice in form.backend.sound_backends() {
                                    ui.selectable_value(
                                        &mut form.sound_backend,
                                        choice,
                                        choice.to_string(),
                                    )
                                    .on_hover_text(sound_backend_hint(choice));
                                }
                            },
                        );
                    },
                );
            }
        });
        if let Some(reason) = form
            .sound
            .then(|| form.backend.sound_backend_block(form.sound_backend))
            .flatten()
        {
            ui.label(RichText::new(reason.short).color(theme::WARN).size(12.0));
        }
    }
}

/// The forward rules as sentences, each with a remove button, and a small form
/// that writes a new one — nobody has to know the `2222:22` syntax.
fn port_forwards(ui: &mut egui::Ui, form: &mut EditForm) {
    let draft_id = egui::Id::new(("forward-draft", form.name.as_str()));
    let mut draft: ForwardDraft = ui.data(|d| d.get_temp(draft_id)).unwrap_or_default();
    let full = form.forwards.len() >= control_api::MAX_PORT_FORWARDS;
    let mut remove = None;
    rows::group(ui, |g| {
        g.row(
            "Port forwarding",
            "Reach a service inside the machine from this computer.",
            FORWARD_TIP,
            |_| {},
        );
        for (index, rule) in form.forwards.iter_mut().enumerate() {
            g.body(|ui| {
                ui.horizontal(|ui| match control_api::PortForwardRule::parse(rule) {
                    Ok(parsed) => {
                        ui::chip(ui, if parsed.udp { "UDP" } else { "TCP" }, theme::TEXT_DIM);
                        ui.add_space(4.0);
                        let local = parsed.bind.is_loopback();
                        ui.label(
                            RichText::new(format!("port {}", parsed.host_port))
                                .size(13.5)
                                .color(theme::TEXT),
                        );
                        ui.label(ui::faint(if local {
                            "on this computer"
                        } else {
                            "open to your network"
                        }));
                        ui.label(RichText::new("  to  ").size(12.0).color(theme::TEXT_FAINT));
                        ui.label(
                            RichText::new(format!("port {} in the guest", parsed.guest_port))
                                .size(13.5)
                                .color(theme::TEXT),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui::icon_button(ui, Icon::Close, "Remove this forward").clicked() {
                                remove = Some(index);
                            }
                        });
                    }
                    // A hand-written rule that does not parse stays editable
                    // as text, with the reason beside it.
                    Err(error) => {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(rule)
                                        .desired_width(240.0)
                                        .hint_text("host port:guest port"),
                                );
                                if ui::icon_button(ui, Icon::Close, "Remove this forward").clicked()
                                {
                                    remove = Some(index);
                                }
                            });
                            ui.label(RichText::new(error).color(theme::WARN).size(11.5));
                        });
                    }
                });
            });
        }
        if draft.open {
            g.body(|ui| {
                ui.horizontal(|ui| {
                    rows::segmented(ui, &mut draft.udp, &[(false, "TCP"), (true, "UDP")]);
                    ui.add_space(6.0);
                    ui.label(ui::dim("Port"));
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.host)
                            .desired_width(64.0)
                            .hint_text("2222"),
                    );
                    ui.label(ui::dim("here to port"));
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.guest)
                            .desired_width(64.0)
                            .hint_text("22"),
                    );
                    ui.label(ui::dim("in the guest"));
                });
                ui.horizontal(|ui| {
                    rows::switch(ui, &mut draft.lan, "Open to my network");
                    ui.label(ui::dim("Also reachable from other computers on my network"));
                });
                ui.horizontal(|ui| {
                    let rule = draft.rule();
                    if ui::secondary_button(ui, "Add", rule.is_some()).clicked() {
                        if let Some(rule) = rule {
                            form.forwards.push(rule);
                            draft = ForwardDraft::default();
                        }
                    }
                    if ui::text_button(ui, "Cancel", theme::TEXT_DIM).clicked() {
                        draft = ForwardDraft::default();
                    }
                });
            });
        } else if g.action(Icon::Plus, "Add port forward", !full).clicked() {
            draft.open = true;
        }
    });
    if let Some(index) = remove {
        form.forwards.remove(index);
    }
    ui.data_mut(|d| d.insert_temp(draft_id, draft));
}

/// "7 of 10 device slots used", as a row of cells and the devices named.
fn device_slots(ui: &mut egui::Ui, used: &[&'static str], max: usize) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Device slots").size(13.0).color(theme::TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(ui::dim(format!("{} of {max} used", used.len())));
        });
    });
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 8.0), egui::Sense::hover());
    let cells = max.max(used.len()).max(1);
    let cell_w = (rect.width() - 4.0 * (cells as f32 - 1.0)) / cells as f32;
    for i in 0..cells {
        let x = rect.left() + i as f32 * (cell_w + 4.0);
        let cell = egui::Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(cell_w, 8.0));
        let color = if i >= max {
            theme::ERR
        } else if i < used.len() {
            theme::ACCENT_DEEP
        } else {
            theme::STROKE
        };
        ui.painter()
            .rect_filled(cell, egui::CornerRadius::same(2), color);
    }
    let mut names: Vec<(String, usize)> = Vec::new();
    for device in used {
        let name = device_name(device).to_string();
        match names.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => names.push((name, 1)),
        }
    }
    let list = names
        .iter()
        .map(|(name, count)| {
            if *count > 1 {
                format!("{name} ×{count}")
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let free = max.saturating_sub(used.len());
    let tail = match free {
        0 => "nothing free".to_string(),
        1 => "1 free".to_string(),
        n => format!("{n} free"),
    };
    ui.add(egui::Label::new(ui::faint(format!("{list} — {tail}"))).wrap());
}

/// An engine device name as the person knows the thing.
fn device_name(device: &str) -> &str {
    match device {
        "disk" => "Disk",
        "cdrom" => "Disc drive",
        "network" => "Network",
        "gpu" => "Screen",
        "render node" => "3D",
        "keyboard" => "Keyboard",
        "tablet" => "Pointer",
        "mouse" => "Mouse",
        "sound" => "Sound",
        "gamepad" => "Gamepad",
        "share" => "Shared folder",
        "memory reclaim" => "Memory return",
        "agent console" => "Guest agent",
        "optical drives" => "Disc drives",
        other => other,
    }
}

/// The explanation on the port-forward row.
const FORWARD_TIP: &str = "Reach a service inside the machine from this computer: port 2222 \
     here reaching the guest's SSH on port 22, for example. It listens on this computer \
     only, unless you open it to your network. UDP is for a game server or anything else \
     that speaks it. Changes apply the next time the machine starts.";

/// One sentence per sound backend, for the hover in the picker.
fn sound_backend_hint(backend: control_api::SoundBackend) -> &'static str {
    use control_api::SoundBackend;
    match backend {
        SoundBackend::Auto => {
            "Whatever this computer has, and silence if it has nothing. Never prevents a \
             machine from starting."
        }
        SoundBackend::Null => {
            "A card that plays into nothing. The guest still sees and uses a working sound \
             device — useful for a machine that should stay quiet."
        }
        SoundBackend::Alsa => {
            "Linux's own audio system, opened directly. Says so and stops if this computer \
             has no ALSA."
        }
        SoundBackend::Wasapi => {
            "Windows' own audio system, shared with everything else that is playing. Says \
             so and stops if this computer has no playback device."
        }
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

fn edit_storage(ui: &mut egui::Ui, form: &mut EditForm, actions: &mut Vec<Action>) {
    // The picker writes into these buffers; what comes back is attached at
    // once, because choosing the file *is* the decision.
    let picked = form.add_disk.trim().to_string();
    if !picked.is_empty() {
        form.disks.push(control_api::DiskSection {
            path: std::path::PathBuf::from(picked),
            writable: true,
        });
        form.add_disk.clear();
    }
    let picked = form.add_share.trim().to_string();
    if !picked.is_empty() && form.shares.len() < control_api::MAX_SHARES {
        add_share(form, std::path::PathBuf::from(picked));
    }
    form.add_share.clear();

    rows::section_title(ui, "Disks");
    let mut remove = None;
    if form.disks.is_empty() {
        rows::group(ui, |g| {
            g.row(
                "No disks",
                "Attach a disk image for the machine to start from.",
                "",
                |_| {},
            );
        });
    } else {
        rows::group(ui, |g| {
            for (index, disk) in form.disks.iter_mut().enumerate() {
                let path = disk.path.display().to_string();
                let title = disk
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                let resolved = form.work_dir.join(&disk.path);
                let size = std::fs::metadata(&resolved)
                    .map(|m| crate::discovery::format_bytes(m.len()))
                    .ok();
                let role = if index == 0 {
                    "Startup disk"
                } else {
                    "Extra disk"
                };
                let detail = match size {
                    Some(size) => format!("{role} · {size} · {path}"),
                    None => format!("{role} · file not found · {path}"),
                };
                let mut read_only = !disk.writable;
                g.row(
                    &title,
                    &detail,
                    "The guest sees disks in this order, the first as the one it starts from \
                     (/dev/vda, then /dev/vdb…). Read-only lets the guest read the disk but \
                     never change it.",
                    |ui| {
                        if ui::icon_button(
                            ui,
                            Icon::Close,
                            "Detach from this machine (the file stays)",
                        )
                        .clicked()
                        {
                            remove = Some(index);
                        }
                        ui.add_space(4.0);
                        rows::switch(ui, &mut read_only, "Read-only");
                        ui.label(ui::dim("Read-only"));
                    },
                );
                disk.writable = !read_only;
            }
        });
    }
    if let Some(index) = remove {
        form.disks.remove(index);
    }
    ui.horizontal(|ui| {
        if ui::secondary_button(ui, "Attach a disk image…", true)
            .on_hover_text("An existing image file; it is used where it is, not copied")
            .clicked()
        {
            actions.push(Action::PickPath(PickTarget::EditorAddDisk));
        }
    });
    ui.label(ui::faint(
        "Move, resize or delete disk images from Storage in the sidebar.",
    ));

    gap(ui);
    rows::section_title(ui, "Shared folders");
    let windows = form.windows_guest();
    let mut remove = None;
    let mut bad_tag = false;
    if !form.shares.is_empty() {
        rows::group(ui, |g| {
            for (index, share) in form.shares.iter_mut().enumerate() {
                let path = share.path.display().to_string();
                let title = share
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                let place = if windows && share.virtiofs {
                    if index == 0 {
                        "Drive Z: in Windows".to_string()
                    } else {
                        "Not mounted automatically".to_string()
                    }
                } else {
                    format!("Mounts as \"{}\"", share.tag)
                };
                bad_tag |= !control_api::valid_share_tag(&share.tag);
                g.row(
                    &title,
                    &format!("{place} · {path}"),
                    if windows {
                        SHARE_TIP_WINDOWS
                    } else {
                        SHARE_TIP
                    },
                    |ui| {
                        if ui::icon_button(ui, Icon::Close, "Stop sharing this folder").clicked() {
                            remove = Some(index);
                        }
                        ui.add_space(4.0);
                        rows::switch(ui, &mut share.read_only, "Read-only");
                        ui.label(ui::dim("Read-only"));
                        ui.add_space(8.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut share.tag)
                                .desired_width(96.0)
                                .hint_text("name"),
                        )
                        .on_hover_text("The name the guest mounts it by");
                    },
                );
            }
        });
    }
    if let Some(index) = remove {
        form.shares.remove(index);
    }
    if bad_tag {
        ui.label(
            RichText::new("A share's name is letters, digits, '-', '_' or '.', up to 31 of them.")
                .color(theme::WARN)
                .size(12.0),
        );
    }
    let full = form.shares.len() >= control_api::MAX_SHARES;
    ui.horizontal(|ui| {
        if ui::secondary_button(ui, "Share a folder…", !full)
            .on_hover_text(if full {
                "That is as many as a machine can have"
            } else {
                "A folder on this computer the machine can open as a drive of its own"
            })
            .clicked()
        {
            actions.push(Action::PickPath(PickTarget::EditorAddShare));
        }
    });
    if windows {
        ui.label(ui::faint(
            "Windows opens the first shared folder as drive Z: every time it starts.",
        ));
    } else if let Some(first) = form.shares.first() {
        ui.label(ui::faint("In a Linux guest, mount one with:"));
        ui.add(
            egui::Label::new(
                RichText::new(format!(
                    "sudo mount -t 9p -o trans=virtio {0} /mnt/{0}",
                    first.tag
                ))
                .monospace()
                .size(12.0)
                .color(theme::TEXT_DIM),
            )
            .selectable(true),
        );
    }
}

/// A folder shared under a tag of its own: the folder's name, numbered if
/// taken.
fn add_share(form: &mut EditForm, path: std::path::PathBuf) {
    let base = control_api::share_tag_for(&path);
    let mut tag = base.clone();
    let mut n = 2;
    while form.shares.iter().any(|s| s.tag == tag) {
        tag = format!("{}-{n}", &base[..base.len().min(28)]);
        n += 1;
    }
    let windows = form.windows_guest();
    form.shares.push(control_api::ShareSection {
        path,
        tag,
        read_only: false,
        // Windows mounts virtio-fs; everything else 9P.
        virtiofs: windows,
    });
}

/// The explanation on the shared-folder rows.
const SHARE_TIP: &str = "A folder on this computer the machine can open as a drive of \
     its own. Each one is mounted inside the guest by its name, for example \
     `sudo mount -t 9p -o trans=virtio <name> /mnt/<name>` (or a line in /etc/fstab). \
     Read-only lets the guest look but never change anything. Files appear to belong to \
     whoever looks at them in the guest; what they can really do is what this computer \
     lets Entangled do.";

/// The same, for a Windows guest (virtio-fs through WinFsp).
const SHARE_TIP_WINDOWS: &str = "A folder on this computer that Windows opens as a drive \
     of its own: the first one is Z:, mounted at every start by the virtio-fs service \
     the installer set up (WinFsp). Read-only lets the guest look but never change \
     anything; what Windows can really do is what this computer lets Entangled do.";

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

fn edit_startup(ui: &mut egui::Ui, form: &mut EditForm, actions: &mut Vec<Action>) {
    use control_api::BootMode;

    rows::section_title(ui, &format!("How {} starts", form.name));
    ui.horizontal(|ui| {
        let gap = ui.spacing().item_spacing.x;
        let card_w = ((ui.available_width() - gap) / 2.0).floor();
        for (mode, title, detail, hint) in [
            (
                BootMode::Uefi,
                "Like a PC (UEFI)",
                "Its own boot menu, and it can boot\nan installer disc.",
                "UEFI firmware behaves like a real PC: the machine has its own boot menu and \
                 can boot an installer disc.",
            ),
            (
                BootMode::DirectLinux,
                "Straight into a Linux kernel",
                "Faster, but boots only the one\nkernel you pick.",
                "Direct Linux skips the firmware and hands a Linux kernel straight to the \
                 machine — faster, but it can only boot that one kernel.",
            ),
        ] {
            if option_card(
                ui,
                card_w,
                title,
                detail,
                form.boot_mode == mode,
                true,
                theme::ACCENT,
            )
            .on_hover_text(hint)
            .clicked()
            {
                form.boot_mode = mode;
            }
        }
    });
    gap(ui);
    match form.boot_mode {
        BootMode::Uefi => startup_uefi(ui, form, actions),
        BootMode::DirectLinux => startup_direct(ui, form, actions),
        // A BIOS machine has nothing to choose here: it boots its disc, then
        // its disk. The profile is written by hand for now (ADR-0012).
        BootMode::Bios => {
            ui.label("This machine starts through a BIOS: it boots its disc, then its disk.");
        }
    }
}

fn startup_uefi(ui: &mut egui::Ui, form: &mut EditForm, actions: &mut Vec<Action>) {
    rows::section_title(ui, "Installer disc");
    let disc = form.cdrom.trim().to_string();
    rows::group(ui, |g| {
        if disc.is_empty() {
            g.row(
                "No disc inserted",
                "Insert an .iso to install a system from it.",
                "An installer disc image (.iso) in this machine's virtual optical drive.",
                |ui| {
                    if ui::secondary_button(ui, "Insert a disc…", true).clicked() {
                        actions.push(Action::PickPath(PickTarget::EditorCdrom));
                    }
                },
            );
        } else {
            let path = std::path::PathBuf::from(&disc);
            let title = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| disc.clone());
            let detail = match std::fs::metadata(form.resolve(&disc)) {
                Ok(meta) => format!(
                    "{} · {}",
                    crate::discovery::format_bytes(meta.len()),
                    path.parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
                Err(_) => format!("File not found · {disc}"),
            };
            g.row(
                &title,
                &detail,
                "An installer disc image (.iso) in this machine's virtual optical drive. \
                 Eject it once the system is installed.",
                |ui| {
                    if ui::secondary_button(ui, "Eject", true).clicked() {
                        form.cdrom.clear();
                    }
                    if ui::text_button(ui, "Change…", theme::TEXT_DIM).clicked() {
                        actions.push(Action::PickPath(PickTarget::EditorCdrom));
                    }
                },
            );
        }
    });
    if !disc.is_empty() {
        ui.label(ui::faint(format!(
            "Eject it once the system is installed, so {} starts from its disk.",
            form.name
        )));
    }

    gap(ui);
    rows::section_title(ui, "Firmware");
    rows::group(ui, |g| {
        let firmware = form.firmware.trim().to_string();
        let (detail, word, color, fix) = if firmware.is_empty() {
            (
                "Not set — a UEFI machine cannot start without it.".to_string(),
                "Not set",
                theme::WARN,
                true,
            )
        } else if form.resolve(&firmware).is_file() {
            (
                "The same for every machine; it comes with Entangled.".to_string(),
                "Ready",
                theme::OK,
                false,
            )
        } else {
            // A profile carries the firmware path of the computer that created
            // it, so on a fresh installation it often names nothing — and
            // Entangled falls through to the copy this computer has rather than
            // refusing (apps/entangled/src/firmware.rs). Saying "missing" there
            // would describe a failure that will not happen.
            match launcher::locate_firmware(&form.work_dir) {
                Some((_, origin)) => (
                    format!("Starts on the copy from {}.", origin.label()),
                    "Ready",
                    theme::OK,
                    false,
                ),
                None => (
                    launcher::FIRMWARE_FIX.to_string(),
                    "Missing",
                    theme::ERR,
                    false,
                ),
            }
        };
        g.row(
            "UEFI firmware",
            &detail,
            "The UEFI firmware this machine boots — the equivalent of a PC's BIOS. It is the \
             same file for every machine and ships with Entangled.",
            |ui| {
                if fix && ui::secondary_button(ui, "Use the built-in one", true).clicked() {
                    form.firmware = form.suggested_firmware().display().to_string();
                }
                rows::status(ui, word, color);
            },
        );
        let nvram = form.nvram.trim().to_string();
        let (detail, word, color) = if nvram.is_empty() {
            (
                "Not kept — the machine forgets its boot entries between runs.".to_string(),
                "Not kept",
                theme::WARN,
            )
        } else if form.resolve(&nvram).is_file() {
            (
                format!(
                    "{}'s own boot entries, kept beside its disk and moved with it.",
                    form.name
                ),
                "Saved",
                theme::OK,
            )
        } else {
            (
                format!("Created the first time {} starts.", form.name),
                "New",
                theme::ACCENT,
            )
        };
        g.row(
            "Boot settings",
            &detail,
            "This machine's own UEFI settings and boot entries — what a real PC keeps in a \
             chip on the board. It belongs to this machine: it is copied when the disk is \
             moved and deleted when the machine is deleted. Without it the machine forgets \
             how to boot after an update.",
            |ui| {
                if nvram.is_empty() && ui::secondary_button(ui, "Keep them", true).clicked() {
                    form.nvram = form.suggested_nvram().display().to_string();
                }
                rows::status(ui, word, color);
            },
        );
    });

    gap(ui);
    if rows::disclosure(
        ui,
        "startup-advanced",
        "Advanced",
        "Firmware file · boot settings file",
    ) {
        rows::group(ui, |g| {
            g.body(|ui| {
                ui::form_scope(ui);
                if ui::path_row(
                    ui,
                    "Firmware file",
                    "A UEFI firmware image of your own instead of the one Entangled ships.",
                    &mut form.firmware,
                    launcher::UEFI_FIRMWARE,
                    true,
                ) {
                    actions.push(Action::PickPath(PickTarget::EditorFirmware));
                }
                if ui::path_row(
                    ui,
                    "Boot settings",
                    "Where this machine's UEFI settings file is kept. It normally sits beside \
                     the disk.",
                    &mut form.nvram,
                    "kept beside the disk",
                    true,
                ) {
                    actions.push(Action::PickPath(PickTarget::EditorNvram));
                }
            });
        });
    }
}

fn startup_direct(ui: &mut egui::Ui, form: &mut EditForm, actions: &mut Vec<Action>) {
    rows::section_title(ui, "Linux kernel");
    rows::group(ui, |g| {
        g.body(|ui| {
            ui::form_scope(ui);
            if ui::path_row(
                ui,
                "Kernel",
                "The Linux kernel image this machine boots. With no firmware in the way, \
                 this file *is* the boot process.",
                &mut form.kernel,
                launcher::BOOTSTRAP_KERNEL,
                true,
            ) {
                actions.push(Action::PickPath(PickTarget::EditorKernel));
            }
            if !form.kernel.trim().is_empty() {
                ui::path_status_note(ui, &form.resolve(&form.kernel.clone()), "");
            }
            if ui::path_row(
                ui,
                "Initramfs",
                "A small temporary filesystem the kernel unpacks before it can reach the \
                 real disk. Optional — many kernels do not need one.",
                &mut form.initramfs,
                "optional",
                true,
            ) {
                actions.push(Action::PickPath(PickTarget::EditorInitramfs));
            }
            ui::form_row(
                ui,
                "Boot options",
                "The command line handed to the kernel: which disk holds the system, where \
                 the console goes, and so on.",
                |ui, field_w| {
                    ui.add(
                        egui::TextEdit::singleline(&mut form.cmdline)
                            .desired_width(field_w)
                            .min_size(Vec2::new(0.0, ui::BUTTON_H))
                            .vertical_align(Align::Center)
                            .hint_text("console=ttyS0 root=…"),
                    );
                },
            );
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forward_draft_writes_the_profile_syntax() {
        let mut draft = ForwardDraft {
            host: "2222".into(),
            guest: "22".into(),
            ..Default::default()
        };
        assert_eq!(draft.rule().as_deref(), Some("2222:22"));
        draft.udp = true;
        draft.lan = true;
        assert_eq!(draft.rule().as_deref(), Some("udp:0.0.0.0:2222:22"));
        draft.guest = "0".into();
        assert_eq!(draft.rule(), None, "port 0 is not a port");
        draft.guest = "ssh".into();
        assert_eq!(draft.rule(), None);
    }

    #[test]
    fn memory_reads_in_gigabytes() {
        assert_eq!(gb(8192), "8 GB");
        assert_eq!(gb(1536), "1.5 GB");
    }
}
