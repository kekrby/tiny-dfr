use std::{
    fs::{File, OpenOptions, read_to_string},
    os::{
        fd::AsRawFd,
        unix::{io::OwnedFd, fs::OpenOptionsExt}
    },
    path::Path,
    collections::HashMap,
};
use cairo::{
    ImageSurface, Format, Context,
    FontSlant, FontWeight, Rectangle
};
use rsvg::{Loader, CairoRenderer, SvgHandle};
use drm::control::ClipRect;
use anyhow::Result;
use input::{
    Libinput, LibinputInterface, Device as InputDevice,
    event::{
        Event, device::DeviceEvent, EventTrait,
        touch::{TouchEvent, TouchEventPosition, TouchEventSlot},
        keyboard::{KeyboardEvent, KeyboardEventTrait, KeyState}
    }
};
use libc::{O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY, c_char};
use input_linux::{uinput::UInputHandle, EventKind, Key, SynchronizeKind};
use input_linux_sys::{uinput_setup, input_id, timeval, input_event};
use nix::poll::{poll, PollFd, PollFlags};
use privdrop::PrivDrop;
use serde::Deserialize;

mod backlight;
mod display;

use backlight::BacklightManager;
use display::DrmBackend;

const BUTTON_COLOR_INACTIVE: f64 = 0.200;
const BUTTON_COLOR_ACTIVE: f64 = 0.400;
const TIMEOUT_MS: i32 = 30 * 1000;

enum ButtonImage {
    Text(&'static str),
    Svg(SvgHandle)
}

struct Button {
    image: ButtonImage,
    changed: bool,
    active: bool,
    action: Key
}

impl Button {
    fn new_text(text: &'static str, action: Key) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Text(text)
        }
    }
    fn new_svg(path: &'static str, action: Key) -> Button {
        let svg = Loader::new().read_path(format!("/usr/share/tiny-dfr/{}.svg", path)).unwrap();
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Svg(svg)
        }
    }
    fn render(&self, c: &Context, height: f64, left_edge: f64, button_width: f64) {
        match &self.image {
            ButtonImage::Text(text) => {
                let extents = c.text_extents(text).unwrap();
                c.move_to(
                    left_edge + button_width / 2.0 - extents.width() / 2.0,
                    height / 2.0 + extents.height() / 2.0
                );
                c.show_text(text).unwrap();
            },
            ButtonImage::Svg(svg) => {
                let renderer = CairoRenderer::new(&svg);
                let y = 0.10 * height;
                let size = height - y * 2.0;
                let x = left_edge + button_width / 2.0 - size / 2.0;
                renderer.render_document(c,
                    &Rectangle::new(x, y, size, size)
                ).unwrap();
            }
        }
    }
    fn set_active<F>(&mut self, uinput: &mut UInputHandle<F>, active: bool) where F: AsRawFd {
        if self.active != active {
            self.active = active;
            self.changed = true;

            toggle_key(uinput, self.action, active as i32);
        }
    }
}

struct FunctionLayer {
    buttons: Vec<Button>
}

impl FunctionLayer {
    fn draw(&mut self, surface: &ImageSurface, complete_redraw: bool) -> Vec<ClipRect> {
        let c = Context::new(&surface).unwrap();
        let mut modified_regions = Vec::new();
        let height = surface.width();
        let width = surface.height();
        c.translate(height as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        let button_width = width as f64 / (self.buttons.len() + 1) as f64;
        let spacing_width = (width as f64 - self.buttons.len() as f64 * button_width) / (self.buttons.len() - 1) as f64;
        let radius = 8.0f64;
        let bot = (height as f64) * 0.15;
        let top = (height as f64) * 0.85;
        if complete_redraw {
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.paint().unwrap();
        }
        c.select_font_face("sans-serif", FontSlant::Normal, FontWeight::Normal);
        c.set_font_size(32.0);
        for (i, button) in self.buttons.iter_mut().enumerate() {
            if !button.changed && !complete_redraw {
                continue;
            };

            let left_edge = i as f64 * (button_width + spacing_width);
            if !complete_redraw {
                c.set_source_rgb(0.0, 0.0, 0.0);
                c.rectangle(left_edge, bot - radius, button_width, top - bot + radius * 2.0);
                c.fill().unwrap();
            }
            let color = if button.active { BUTTON_COLOR_ACTIVE } else { BUTTON_COLOR_INACTIVE };
            c.set_source_rgb(color, color, color);
            // draw box with rounded corners
            c.new_sub_path();
            let left = left_edge + radius;
            let right = left_edge + button_width - radius;
            c.arc(
                right,
                bot,
                radius,
                (-90.0f64).to_radians(),
                (0.0f64).to_radians(),
            );
            c.arc(
                right,
                top,
                radius,
                (0.0f64).to_radians(),
                (90.0f64).to_radians(),
            );
            c.arc(
                left,
                top,
                radius,
                (90.0f64).to_radians(),
                (180.0f64).to_radians(),
            );
            c.arc(
                left,
                bot,
                radius,
                (180.0f64).to_radians(),
                (270.0f64).to_radians(),
            );
            c.close_path();

            c.fill().unwrap();
            c.set_source_rgb(1.0, 1.0, 1.0);
            button.render(&c, height as f64, left_edge, button_width);

            button.changed = false;
            modified_regions.push(ClipRect {
                x1: height as u16 - top as u16 - radius as u16,
                y1: left_edge as u16,
                x2: height as u16 - bot as u16 + radius as u16,
                y2: left_edge as u16 + button_width as u16
            });
        }

        if complete_redraw {
            vec![ClipRect {
                x1: 0,
                y1: 0,
                x2: height as u16,
                y2: width as u16,
            }]
        } else {
            modified_regions
        }
    }
}

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let mode = flags & O_ACCMODE;

        OpenOptions::new()
            .custom_flags(flags)
            .read(mode == O_RDONLY || mode == O_RDWR)
            .write(mode == O_WRONLY || mode == O_RDWR)
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        _ = File::from(fd);
    }
}


fn button_hit(num: u32, idx: u32, width: u16, height: u16, x: f64, y: f64) -> bool {
    let button_width = width as f64 / (num + 1) as f64;
    let spacing_width = (width as f64 - num as f64 * button_width) / (num - 1) as f64;
    let left_edge = idx as f64 * (button_width + spacing_width);
    if x < left_edge || x > (left_edge + button_width) {
        return false
    }
    y > 0.09 * height as f64 && y < 0.91 * height as f64
}

fn emit<F>(uinput: &mut UInputHandle<F>, ty: EventKind, code: u16, value: i32) where F: AsRawFd {
    uinput.write(&[input_event {
        value: value,
        type_: ty as u16,
        code: code,
        time: timeval {
            tv_sec: 0,
            tv_usec: 0
        }
    }]).unwrap();
}

fn toggle_key<F>(uinput: &mut UInputHandle<F>, code: Key, value: i32) where F: AsRawFd {
    emit(uinput, EventKind::Key, code as u16, value);
    emit(uinput, EventKind::Synchronize, SynchronizeKind::Report as u16, 0);
}

#[repr(usize)]
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LayerType {
    Function,
    Special,
}

#[derive(Deserialize)]
struct UiConfig {
    primary_layer: LayerType,
    secondary_layer: LayerType,
}

#[derive(Deserialize)]
struct Config {
    ui: UiConfig,
}

impl Config {
    fn from_file(path: &str) -> Result<Self> {
        toml::from_str(&read_to_string(path)?)
            .map_err(anyhow::Error::from)
    }
}

fn main() {
    let config = Config::from_file("/etc/tiny-dfr.conf").unwrap();
    let mut uinput = UInputHandle::new(OpenOptions::new().write(true).open("/dev/uinput").unwrap());
    let mut backlight = BacklightManager::new();

    // drop privileges to input and video group
    let groups = ["input", "video"];

    PrivDrop::default()
        .user("nobody")
        .group("nobody")
        .group_list(&groups)
        .apply()
        .unwrap_or_else(|e| { panic!("Failed to drop privileges: {}", e) });

    let mut active_layer = config.ui.primary_layer as usize;
    let mut layers = [
        FunctionLayer {
            buttons: vec![
                Button::new_text("F1", Key::F1),
                Button::new_text("F2", Key::F2),
                Button::new_text("F3", Key::F3),
                Button::new_text("F4", Key::F4),
                Button::new_text("F5", Key::F5),
                Button::new_text("F6", Key::F6),
                Button::new_text("F7", Key::F7),
                Button::new_text("F8", Key::F8),
                Button::new_text("F9", Key::F9),
                Button::new_text("F10", Key::F10),
                Button::new_text("F11", Key::F11),
                Button::new_text("F12", Key::F12)
            ]
        },
        FunctionLayer {
            buttons: vec![
                Button::new_svg("brightness_low", Key::BrightnessDown),
                Button::new_svg("brightness_high", Key::BrightnessUp),
                Button::new_svg("mic_off", Key::MicMute),
                Button::new_svg("search", Key::Search),
                Button::new_svg("backlight_low", Key::IllumDown),
                Button::new_svg("backlight_high", Key::IllumUp),
                Button::new_svg("fast_rewind", Key::PreviousSong),
                Button::new_svg("play_pause", Key::PlayPause),
                Button::new_svg("fast_forward", Key::NextSong),
                Button::new_svg("volume_off", Key::Mute),
                Button::new_svg("volume_down", Key::VolumeDown),
                Button::new_svg("volume_up", Key::VolumeUp)
            ]
        }
    ];

    let mut needs_complete_redraw = true;
    let mut drm = DrmBackend::open_card().unwrap();
    let (height, width) = drm.mode().size();
    let fb_info = drm.fb_info().unwrap();
    let pitch = fb_info.pitch();
    let cpp = fb_info.bpp() / 8;

    let mut surface = ImageSurface::create(Format::ARgb32, height as i32, width as i32).unwrap();
    let mut input_tb = Libinput::new_with_udev(Interface);
    let mut input_main = Libinput::new_with_udev(Interface);
    input_tb.udev_assign_seat("seat-touchbar").unwrap();
    input_main.udev_assign_seat("seat0").unwrap();
    let pollfd_tb = PollFd::new(input_tb.as_raw_fd(), PollFlags::POLLIN);
    let pollfd_main = PollFd::new(input_main.as_raw_fd(), PollFlags::POLLIN);
    uinput.set_evbit(EventKind::Key).unwrap();
    for layer in &layers {
        for button in &layer.buttons {
            uinput.set_keybit(button.action).unwrap();
        }
    }
    let mut dev_name_c = [0 as c_char; 80];
    let dev_name = "Dynamic Function Row Virtual Input Device".as_bytes();
    for i in 0..dev_name.len() {
        dev_name_c[i] = dev_name[i] as c_char;
    }
    uinput.dev_setup(&uinput_setup {
        id: input_id {
            bustype: 0x19,
            vendor: 0x1209,
            product: 0x316E,
            version: 1
        },
        ff_effects_max: 0,
        name: dev_name_c
    }).unwrap();
    uinput.dev_create().unwrap();

    let mut digitizer: Option<InputDevice> = None;
    let mut touches = HashMap::new();
    loop {
        if needs_complete_redraw || layers[active_layer].buttons.iter().any(|b| b.changed) {
            let clips = layers[active_layer].draw(&surface, needs_complete_redraw);
            let data = surface.data().unwrap();
            let mut fb = drm.map().unwrap();

            for clip in &clips {
                let base_offset = clip.y1 as usize * pitch as usize + clip.x1 as usize * cpp as usize;
                let len = (clip.x2 - clip.x1) as usize * cpp as usize;

                for i in 0..(clip.y2 - clip.y1) {
                    let offset = base_offset + i as usize * pitch as usize;
                    let range = offset..(offset + len);
                    fb.as_mut()[range.clone()].copy_from_slice(&data[range]);
                }
            }

            drop(fb);
            drm.dirty(&clips[..]).unwrap();
            needs_complete_redraw = false;
        }
        poll(&mut [pollfd_tb, pollfd_main], TIMEOUT_MS).unwrap();
        input_tb.dispatch().unwrap();
        input_main.dispatch().unwrap();
        for event in &mut input_tb.clone().chain(input_main.clone()) {
            backlight.process_event(&event);
            match event {
                Event::Device(DeviceEvent::Added(evt)) => {
                    let dev = evt.device();
                    if dev.name().contains(" Touch Bar") {
                        digitizer = Some(dev);
                    }
                },
                Event::Keyboard(KeyboardEvent::Key(key)) => {
                    if key.key() == Key::Fn as u32 {
                        let new_layer = match key.key_state() {
                            KeyState::Pressed => config.ui.secondary_layer as usize,
                            KeyState::Released => config.ui.primary_layer as usize
                        };
                        if active_layer != new_layer {
                            active_layer = new_layer;
                            needs_complete_redraw = true;
                        }
                    }
                },
                Event::Touch(te) => {
                    if Some(te.device()) != digitizer || backlight.current_bl() == 0 {
                        continue
                    }
                    match te {
                        TouchEvent::Down(dn) => {
                            let x = dn.x_transformed(width as u32);
                            let y = dn.y_transformed(height as u32);
                            let btn = (x / (width as f64 / layers[active_layer].buttons.len() as f64)) as u32;
                            if button_hit(layers[active_layer].buttons.len() as u32, btn, width, height, x, y) {
                                touches.insert(dn.seat_slot(), (active_layer, btn));
                                layers[active_layer].buttons[btn as usize].set_active(&mut uinput, true);
                            }
                        },
                        TouchEvent::Motion(mtn) => {
                            if !touches.contains_key(&mtn.seat_slot()) {
                                continue;
                            }

                            let x = mtn.x_transformed(width as u32);
                            let y = mtn.y_transformed(height as u32);
                            let (layer, btn) = *touches.get(&mtn.seat_slot()).unwrap();
                            let hit = button_hit(layers[layer].buttons.len() as u32, btn, width, height, x, y);
                            layers[layer].buttons[btn as usize].set_active(&mut uinput, hit);
                        },
                        TouchEvent::Up(up) => {
                            if !touches.contains_key(&up.seat_slot()) {
                                continue;
                            }
                            let (layer, btn) = *touches.get(&up.seat_slot()).unwrap();
                            layers[layer].buttons[btn as usize].set_active(&mut uinput, false);
                        }
                        _ => {}
                    }
                },
                _ => {}
            }
        }
        backlight.update_backlight();
    }
}
