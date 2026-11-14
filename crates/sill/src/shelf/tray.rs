//! The tray icon (SPEC §4.6): a StatusNotifierItem — the freedesktop/KDE protocol every current
//! Linux panel takes (Cinnamon, KDE, XFCE, MATE, LXQt, Budgie; GNOME with the AppIndicator
//! extension) — spoken over the application's own D-Bus connection. No appindicator library (they
//! link GTK3, which cannot share a process with GTK4), no XEmbed (GTK4 dropped `GtkStatusIcon`).
//! A left click toggles the shelf; the menu — a minimal `com.canonical.dbusmenu`, which panels
//! expect instead of calling `ContextMenu` — has Show/Hide and Quit. The item registers with the
//! watcher whenever its name appears, so a panel restart brings the icon back; with no watcher
//! there is no icon and the hotkey remains.

use gtk4 as gtk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/StatusNotifierItem/Menu";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
/// The logo, rendered to pixmaps at these sizes: panels pick the nearest and scale.
const LOGO: &str = include_str!("../../../../packaging/icons/sill.svg");
const SIZES: [i32; 4] = [22, 24, 32, 48];

const ITEM_XML: &str = r#"<node><interface name="org.kde.StatusNotifierItem">
 <property name="Category" type="s" access="read"/><property name="Id" type="s" access="read"/>
 <property name="Title" type="s" access="read"/><property name="Status" type="s" access="read"/>
 <property name="WindowId" type="i" access="read"/><property name="IconName" type="s" access="read"/>
 <property name="IconPixmap" type="a(iiay)" access="read"/><property name="OverlayIconName" type="s" access="read"/>
 <property name="OverlayIconPixmap" type="a(iiay)" access="read"/><property name="AttentionIconName" type="s" access="read"/>
 <property name="AttentionIconPixmap" type="a(iiay)" access="read"/><property name="AttentionMovieName" type="s" access="read"/>
 <property name="ToolTip" type="(sa(iiay)ss)" access="read"/><property name="ItemIsMenu" type="b" access="read"/>
 <property name="Menu" type="o" access="read"/>
 <method name="ContextMenu"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
 <method name="Activate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
 <method name="SecondaryActivate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
 <method name="Scroll"><arg name="delta" type="i" direction="in"/><arg name="orientation" type="s" direction="in"/></method>
 <signal name="NewTitle"/><signal name="NewIcon"/><signal name="NewAttentionIcon"/><signal name="NewOverlayIcon"/>
 <signal name="NewToolTip"/><signal name="NewStatus"><arg name="status" type="s"/></signal>
</interface></node>"#;

const MENU_XML: &str = r#"<node><interface name="com.canonical.dbusmenu">
 <property name="Version" type="u" access="read"/><property name="TextDirection" type="s" access="read"/>
 <property name="Status" type="s" access="read"/><property name="IconThemePath" type="as" access="read"/>
 <method name="GetLayout"><arg name="parentId" type="i" direction="in"/><arg name="recursionDepth" type="i" direction="in"/>
  <arg name="propertyNames" type="as" direction="in"/><arg name="revision" type="u" direction="out"/><arg name="layout" type="(ia{sv}av)" direction="out"/></method>
 <method name="GetGroupProperties"><arg name="ids" type="ai" direction="in"/><arg name="propertyNames" type="as" direction="in"/>
  <arg name="properties" type="a(ia{sv})" direction="out"/></method>
 <method name="GetProperty"><arg name="id" type="i" direction="in"/><arg name="name" type="s" direction="in"/><arg name="value" type="v" direction="out"/></method>
 <method name="Event"><arg name="id" type="i" direction="in"/><arg name="eventId" type="s" direction="in"/><arg name="data" type="v" direction="in"/><arg name="timestamp" type="u" direction="in"/></method>
 <method name="EventGroup"><arg name="events" type="a(isvu)" direction="in"/><arg name="idErrors" type="ai" direction="out"/></method>
 <method name="AboutToShow"><arg name="id" type="i" direction="in"/><arg name="needUpdate" type="b" direction="out"/></method>
 <method name="AboutToShowGroup"><arg name="ids" type="ai" direction="in"/><arg name="updatesNeeded" type="ai" direction="out"/><arg name="idErrors" type="ai" direction="out"/></method>
 <signal name="ItemsPropertiesUpdated"><arg type="a(ia{sv})"/><arg type="a(ias)"/></signal>
 <signal name="LayoutUpdated"><arg type="u"/><arg type="i"/></signal>
 <signal name="ItemActivationRequested"><arg type="i"/><arg type="u"/></signal>
</interface></node>"#;

/// Menu items: id, label (empty = separator).
const MENU: &[(i32, &str)] = &[(1, "Show / hide sill"), (2, ""), (3, "Quit sill")];

/// Export the item and its menu, own a name for it, register with the watcher now and whenever
/// it comes back. Errors are logged, never fatal: a shelf without a tray is a shelf.
pub fn start(app: &gtk::Application) {
    let Some(conn) = app.dbus_connection() else {
        eprintln!("sill: tray: no session bus connection");
        return;
    };
    let pixmaps = pixmaps();
    let item = match gio::DBusNodeInfo::for_xml(ITEM_XML).ok().and_then(|n| n.lookup_interface("org.kde.StatusNotifierItem")) {
        Some(i) => i,
        None => return,
    };
    let menu = match gio::DBusNodeInfo::for_xml(MENU_XML).ok().and_then(|n| n.lookup_interface("com.canonical.dbusmenu")) {
        Some(i) => i,
        None => return,
    };
    let r = conn
        .register_object(ITEM_PATH, &item)
        .method_call({
            let app = app.clone();
            move |_, _, _, _, method, _, inv| {
                if matches!(method, "Activate" | "SecondaryActivate" | "ContextMenu") {
                    toggle(&app);
                }
                inv.return_value(None);
            }
        })
        .property(move |_, _, _, _, name| item_property(name, &pixmaps))
        .build();
    if let Err(e) = r {
        eprintln!("sill: tray: {e}");
        return;
    }
    let r = conn
        .register_object(MENU_PATH, &menu)
        .method_call({
            let app = app.clone();
            move |_, _, _, _, method, params, inv| menu_call(&app, method, &params, inv)
        })
        .property(|_, _, _, _, name| match name {
            "Version" => 3u32.to_variant(),
            "Status" => "normal".to_variant(),
            "IconThemePath" => Vec::<String>::new().to_variant(),
            _ => "ltr".to_variant(),
        })
        .build();
    if let Err(e) = r {
        eprintln!("sill: tray: menu: {e}");
        return;
    }
    // the spec's name form; the watcher is handed it and asks us for the properties
    let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    gio::bus_own_name_on_connection(&conn, &name, gio::BusNameOwnerFlags::NONE, |_, _| {}, |_, n| eprintln!("sill: tray: lost {n}"));
    gio::bus_watch_name_on_connection(
        &conn,
        WATCHER,
        gio::BusNameWatcherFlags::NONE,
        move |conn, _, _| {
            conn.call(
                Some(WATCHER),
                "/StatusNotifierWatcher",
                WATCHER,
                "RegisterStatusNotifierItem",
                Some(&(name.as_str(),).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
                |r| {
                    if let Err(e) = r {
                        eprintln!("sill: tray: register: {e}");
                    }
                },
            );
        },
        |_, _| {},
    );
}

fn toggle(app: &gtk::Application) {
    if let Some(w) = app.windows().first() {
        super::window::toggle(w, 0);
    }
}

fn item_property(name: &str, pixmaps: &glib::Variant) -> glib::Variant {
    match name {
        "Category" => "ApplicationStatus".to_variant(),
        "Id" | "Title" => "sill".to_variant(),
        "Status" => "Active".to_variant(),
        "WindowId" => 0i32.to_variant(),
        "IconPixmap" => pixmaps.clone(),
        "OverlayIconPixmap" | "AttentionIconPixmap" => empty_pixmaps(),
        "ToolTip" => glib::Variant::tuple_from_iter(["".to_variant(), empty_pixmaps(), "sill".to_variant(), "Agent shelf · click to show or hide".to_variant()]),
        "ItemIsMenu" => false.to_variant(),
        "Menu" => glib::variant::ObjectPath::try_from(MENU_PATH).expect("a valid path").to_variant(),
        // IconName and the other names: empty, so the pixmap is what shows
        _ => "".to_variant(),
    }
}

fn empty_pixmaps() -> glib::Variant {
    glib::Variant::array_from_iter_with_type(glib::VariantTy::new("(iiay)").expect("a type"), std::iter::empty::<glib::Variant>())
}

/// The logo as `a(iiay)`: ARGB32 in network byte order, straight alpha, one entry per size. An
/// SVG that fails to render (no pixbuf loader) gives an empty list and an iconless item.
fn pixmaps() -> glib::Variant {
    let mut out: Vec<glib::Variant> = Vec::new();
    for size in SIZES {
        let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_static(LOGO.as_bytes()));
        let Ok(pb) = gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale(&stream, size, size, true, None::<&gio::Cancellable>) else {
            continue;
        };
        if pb.n_channels() != 4 {
            continue;
        }
        let (w, h, stride) = (pb.width(), pb.height(), pb.rowstride() as usize);
        let px = pb.read_pixel_bytes();
        let mut argb: Vec<u8> = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            let row = &px[y * stride..y * stride + (w as usize) * 4];
            for p in row.chunks_exact(4) {
                argb.extend_from_slice(&[p[3], p[0], p[1], p[2]]);
            }
        }
        out.push(glib::Variant::tuple_from_iter([w.to_variant(), h.to_variant(), glib::Variant::array_from_fixed_array(&argb)]));
    }
    glib::Variant::array_from_iter_with_type(glib::VariantTy::new("(iiay)").expect("a type"), out)
}

/// One menu item's `a{sv}`.
fn menu_props(id: i32) -> glib::Variant {
    let d = glib::VariantDict::new(None);
    match MENU.iter().find(|(i, _)| *i == id) {
        Some((_, "")) => d.insert_value("type", &"separator".to_variant()),
        Some((_, label)) => {
            d.insert_value("label", &label.to_variant());
            d.insert_value("enabled", &true.to_variant());
            d.insert_value("visible", &true.to_variant());
        }
        None => d.insert_value("children-display", &"submenu".to_variant()), // the root
    }
    d.end()
}

/// `(ia{sv}av)` for the root with every item below it, or for one item alone.
fn layout(id: i32) -> glib::Variant {
    let children: Vec<glib::Variant> = if id == 0 {
        MENU.iter().map(|(i, _)| glib::Variant::from_variant(&layout(*i))).collect()
    } else {
        Vec::new()
    };
    glib::Variant::tuple_from_iter([id.to_variant(), menu_props(id), glib::Variant::array_from_iter_with_type(glib::VariantTy::VARIANT, children)])
}

fn menu_call(app: &gtk::Application, method: &str, params: &glib::Variant, inv: gio::DBusMethodInvocation) {
    let ids_type = glib::VariantTy::new("(ia{sv})").expect("a type");
    match method {
        "GetLayout" => {
            let parent = params.child_value(0).get::<i32>().unwrap_or(0);
            inv.return_value(Some(&glib::Variant::tuple_from_iter([1u32.to_variant(), layout(parent)])));
        }
        "GetGroupProperties" => {
            let ids: Vec<i32> = params.child_value(0).get().unwrap_or_default();
            let ids: Vec<i32> = if ids.is_empty() { MENU.iter().map(|(i, _)| *i).collect() } else { ids };
            let items = ids.into_iter().map(|i| glib::Variant::tuple_from_iter([i.to_variant(), menu_props(i)]));
            inv.return_value(Some(&(glib::Variant::array_from_iter_with_type(ids_type, items),).to_variant()));
        }
        "GetProperty" => {
            let id = params.child_value(0).get::<i32>().unwrap_or(0);
            let name: String = params.child_value(1).get().unwrap_or_default();
            let v = glib::VariantDict::new(Some(&menu_props(id))).lookup_value(&name, None).unwrap_or_else(|| "".to_variant());
            inv.return_value(Some(&(v,).to_variant()));
        }
        "Event" => {
            let id = params.child_value(0).get::<i32>().unwrap_or(0);
            let event: String = params.child_value(1).get().unwrap_or_default();
            if event == "clicked" {
                match id {
                    1 => toggle(app),
                    3 => app.quit(),
                    _ => {}
                }
            }
            inv.return_value(None);
        }
        "EventGroup" => inv.return_value(Some(&(Vec::<i32>::new(),).to_variant())),
        "AboutToShow" => inv.return_value(Some(&(false,).to_variant())),
        "AboutToShowGroup" => inv.return_value(Some(&(Vec::<i32>::new(), Vec::<i32>::new()).to_variant())),
        _ => inv.return_value(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The D-Bus shapes panels parse: the layout tuple, every item's properties, the pixmaps.
    #[test]
    fn shapes_the_panel_expects() {
        let l = layout(0);
        assert_eq!(l.type_().as_str(), "(ia{sv}av)");
        assert_eq!(l.child_value(2).n_children(), MENU.len());
        assert_eq!(menu_props(2).type_().as_str(), "a{sv}");
        assert_eq!(glib::VariantDict::new(Some(&menu_props(1))).lookup_value("label", None).and_then(|v| v.get::<String>()).as_deref(), Some("Show / hide sill"));
        let p = pixmaps();
        assert_eq!(p.type_().as_str(), "a(iiay)");
        // the svg rendered (a pixbuf loader is present): each size, square, 4 bytes a pixel
        assert_eq!(p.n_children(), SIZES.len(), "{p}");
        for (i, size) in SIZES.iter().enumerate() {
            let e = p.child_value(i);
            assert_eq!(e.child_value(0).get::<i32>(), Some(*size));
            assert_eq!(e.child_value(2).n_children(), (*size as usize).pow(2) * 4);
        }
        assert_eq!(item_property("ToolTip", &p).type_().as_str(), "(sa(iiay)ss)");
        assert_eq!(item_property("Menu", &p).type_().as_str(), "o");
    }
}
