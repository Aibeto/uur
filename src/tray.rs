//! Native StatusNotifierItem and DBusMenu, independent of Wine's XEmbed tray.
//! The supervisor owns this helper, so closing the session removes the item.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

type Pixmap = (i32, i32, Vec<u8>);
type Properties = HashMap<String, OwnedValue>;
type Layout = (i32, Properties, Vec<OwnedValue>);

#[derive(Clone, Copy)]
enum Action {
    Show,
    Stop,
}

struct Tray {
    actions: UnboundedSender<Action>,
    icon: Pixmap,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Tray {
    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }
    #[zbus(property)]
    fn id(&self) -> &str {
        "uur"
    }
    #[zbus(property)]
    fn title(&self) -> String {
        t!("tray.title").to_string()
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }
    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        "uur"
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<Pixmap> {
        vec![self.icon.clone()]
    }
    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Vec<Pixmap> {
        Vec::new()
    }
    #[zbus(property)]
    fn attention_movie_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn overlay_icon_pixmap(&self) -> Vec<Pixmap> {
        Vec::new()
    }
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/MenuBar").unwrap()
    }
    #[zbus(property)]
    fn tool_tip(&self) -> (String, Vec<Pixmap>, String, String) {
        (
            "uur".into(),
            vec![self.icon.clone()],
            t!("tray.title").to_string(),
            t!("tray.tooltip").to_string(),
        )
    }

    fn activate(&self, _x: i32, _y: i32) {
        let _ = self.actions.send(Action::Show);
    }
    fn secondary_activate(&self, _x: i32, _y: i32) {
        let _ = self.actions.send(Action::Show);
    }
    // Hosts draw the menu returned by Menu; no X11 popup is necessary.
    fn context_menu(&self, _x: i32, _y: i32) {}
    fn scroll(&self, _delta: i32, _orientation: &str) {}
    fn provide_xdg_activation_token(&self, _token: &str) {}
}

struct Menu {
    actions: UnboundedSender<Action>,
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }
    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Layout)> {
        let properties = menu_properties(parent_id, &property_names)?;
        let mut children = Vec::new();
        if parent_id == 0 && recursion_depth != 0 {
            for id in [1, 2] {
                let child = (
                    id,
                    menu_properties(id, &property_names)?,
                    Vec::<OwnedValue>::new(),
                );
                children.push(
                    OwnedValue::try_from(Value::new(child))
                        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?,
                );
            }
        }
        Ok((1, (parent_id, properties, children)))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<Vec<(i32, Properties)>> {
        let ids = if ids.is_empty() { vec![0, 1, 2] } else { ids };
        ids.into_iter()
            .map(|id| Ok((id, menu_properties(id, &property_names)?)))
            .collect()
    }

    fn get_property(&self, id: i32, name: &str) -> zbus::fdo::Result<OwnedValue> {
        menu_properties(id, &[])?
            .remove(name)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("unknown menu property {name}")))
    }

    fn event(
        &self,
        id: i32,
        event_id: &str,
        _data: OwnedValue,
        _timestamp: u32,
    ) -> zbus::fdo::Result<()> {
        let action = match id {
            1 => Action::Show,
            2 => Action::Stop,
            0 => return Ok(()),
            _ => return Err(zbus::fdo::Error::InvalidArgs("unknown menu item".into())),
        };
        if event_id == "clicked" {
            let _ = self.actions.send(action);
        }
        Ok(())
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        events
            .into_iter()
            .filter_map(|(id, event, data, timestamp)| {
                self.event(id, &event, data, timestamp).err().map(|_| id)
            })
            .collect()
    }

    fn about_to_show(&self, id: i32) -> zbus::fdo::Result<bool> {
        menu_properties(id, &[])?;
        Ok(false)
    }

    fn about_to_show_group(&self, ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (
            Vec::new(),
            ids.into_iter().filter(|id| !(0..=2).contains(id)).collect(),
        )
    }
}

fn menu_properties(id: i32, requested: &[String]) -> zbus::fdo::Result<Properties> {
    let mut properties = Properties::new();
    let string = |value: String| OwnedValue::from(zbus::zvariant::Str::from(value));
    match id {
        0 => {
            properties.insert("children-display".into(), string("submenu".into()));
        }
        1 | 2 => {
            properties.insert(
                "label".into(),
                string(if id == 1 {
                    t!("tray.open").to_string()
                } else {
                    t!("tray.quit").to_string()
                }),
            );
            properties.insert("enabled".into(), OwnedValue::from(true));
            properties.insert("visible".into(), OwnedValue::from(true));
        }
        _ => return Err(zbus::fdo::Error::InvalidArgs("unknown menu item".into())),
    }
    if !requested.is_empty() {
        properties.retain(|key, _| requested.contains(key));
    }
    Ok(properties)
}

fn icon_pixmap() -> Result<Pixmap> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!(
        "../packaging/uur-icon.png"
    )));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("reading tray icon")?;
    let mut pixels = vec![
        0;
        reader
            .output_buffer_size()
            .context("tray icon size overflow")?
    ];
    let info = reader
        .next_frame(&mut pixels)
        .context("decoding tray icon")?;
    anyhow::ensure!(
        info.color_type == png::ColorType::Rgba,
        "tray icon must be RGBA"
    );
    pixels.truncate(info.buffer_size());
    // SNI requires network-byte-order ARGB32, not PNG's RGBA byte order.
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.rotate_right(1);
    }
    Ok((info.width.try_into()?, info.height.try_into()?, pixels))
}

pub fn serve() -> Result<()> {
    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stopping.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stopping.clone())?;
    tokio::runtime::Runtime::new()?.block_on(async {
        let (actions, mut receiver) = unbounded_channel();
        let connection = zbus::connection::Builder::session()?
            .serve_at("/StatusNotifierItem", Tray { actions: actions.clone(), icon: icon_pixmap()? })?
            .serve_at("/MenuBar", Menu { actions })?.build().await?;
        let bus = zbus::fdo::DBusProxy::new(&connection).await?;
        let mut watcher_owner = None;
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                action = receiver.recv() => match action {
                    Some(Action::Show) => {
                        tokio::task::spawn_blocking(|| {
                            if let Err(error) = crate::session::show_client() { eprintln!("tray: {error}"); }
                        });
                    }
                    Some(Action::Stop) => {
                        tokio::task::spawn_blocking(crate::session::stop).await??;
                        break;
                    }
                    None => break,
                },
                _ = timer.tick() => {
                    if stopping.load(Ordering::Relaxed) { break; }
                    let owner = bus.get_name_owner("org.kde.StatusNotifierWatcher".try_into()?).await.ok();
                    if owner != watcher_owner {
                        if owner.is_some() {
                            let watcher = zbus::Proxy::new(&connection, "org.kde.StatusNotifierWatcher",
                                "/StatusNotifierWatcher", "org.kde.StatusNotifierWatcher").await?;
                            match watcher.call::<_, _, ()>("RegisterStatusNotifierItem", &("/StatusNotifierItem",)).await {
                                Ok(()) => {
                                    println!("{}", t!("tray.registered"));
                                    watcher_owner = owner;
                                }
                                Err(error) => eprintln!("tray registration: {error}"),
                            }
                        } else { watcher_owner = None; }
                    }
                }
            }
        }
        Ok(())
    })
}
