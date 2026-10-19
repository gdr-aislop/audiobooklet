//! Mouse-wheel scrolling over rows that scroll sideways (Home's cover shelves, Library's category
//! and genre chips). Each is its own `GtkScrolledWindow` limited to horizontal scrolling; GTK hands
//! it the wheel, it can't scroll vertically, and the page under it never hears of it — so on a
//! computer the wheel did nothing while the pointer was over a row. Now the plain wheel scrolls
//! the page; Shift+wheel, a tilt wheel and a sideways touchpad swipe still scroll the row (GTK's
//! own handling, untouched).
//!
//! Touchscreens are untouched by construction. A scroll-event controller only ever sees scroll
//! events: GTK's `gtk_event_controller_scroll_handle_event` returns at once for anything else
//! (checked in GTK 4.8.3 and 4.14.5), and finger scrolling is done by the scrolled window's own
//! touch-only drag/pan/swipe gestures, which this neither replaces nor reorders. A scroll event
//! that does come from a touchscreen is left alone too, in case some setup produces one.

use gtk4::prelude::*;
use gtk4::{gdk, glib};

/// Makes vertical mouse-wheel and touchpad scrolling over `row` scroll the page it sits in (the
/// nearest scrolled window around it).
pub fn pass_vertical_scroll_to_page(row: &gtk4::ScrolledWindow) {
    add_controller(row, |row| row.parent().and_then(|parent| parent.ancestor(gtk4::ScrolledWindow::static_type())).and_downcast::<gtk4::ScrolledWindow>());
}

/// Like [`pass_vertical_scroll_to_page`], for a row that isn't inside what scrolls — a header
/// above a list: the wheel over it scrolls `page`.
pub fn pass_vertical_scroll_to(row: &gtk4::ScrolledWindow, page: &gtk4::ScrolledWindow) {
    let page = page.downgrade();
    add_controller(row, move |_| page.upgrade());
}

fn add_controller(row: &gtk4::ScrolledWindow, page_of: impl Fn(&gtk4::ScrolledWindow) -> Option<gtk4::ScrolledWindow> + 'static) {
    let controller = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
    // Capture: runs before the row's own scroll handling, which would swallow the event.
    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    controller.connect_scroll({
        let row = row.downgrade();
        move |controller, dx, dy| {
            let Some(row) = row.upgrade() else { return glib::Propagation::Proceed };
            if controller.current_event_device().is_some_and(|device| device.source() == gdk::InputSource::Touchscreen) {
                return glib::Propagation::Proceed;
            }
            let shift = controller.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
            let is_wheel = controller.unit() == gdk::ScrollUnit::Wheel;
            let Some(page) = page_of(&row) else { return glib::Propagation::Proceed };
            let adjustment = page.vadjustment();
            let Some(delta) = route(shift, is_wheel, dx, dy, adjustment.page_size()) else { return glib::Propagation::Proceed };
            let max = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
            adjustment.set_value((adjustment.value() + delta).clamp(adjustment.lower(), max));
            glib::Propagation::Stop
        }
    });
    row.add_controller(controller);
}

/// How far the page should scroll for one scroll event over a sideways row, or `None` to leave the
/// event to the row itself: Shift held (GTK's "scroll sideways" modifier) or a mostly horizontal
/// movement. A wheel moves by GTK's own wheel step for a view of `page_size` (so it feels like the
/// wheel anywhere else on the page); a touchpad's `dy` is already in pixels.
fn route(shift: bool, is_wheel: bool, dx: f64, dy: f64, page_size: f64) -> Option<f64> {
    if shift || dy == 0.0 || dx.abs() >= dy.abs() {
        return None;
    }
    Some(if is_wheel { dy * page_size.max(0.0).powf(2.0 / 3.0) } else { dy })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// One line per controller on `widget`, in order: its type, its propagation phase and, for a
    /// gesture, whether it only takes touch input.
    fn controllers(widget: &impl IsA<gtk4::Widget>) -> Vec<String> {
        let list = widget.as_ref().observe_controllers();
        (0..list.n_items())
            .filter_map(|i| list.item(i).and_downcast::<gtk4::EventController>())
            .map(|controller| {
                let touch_only = controller.downcast_ref::<gtk4::GestureSingle>().map(|gesture| gesture.is_touch_only());
                format!("{} {:?} touch_only={touch_only:?}", controller.type_().name(), controller.propagation_phase())
            })
            .collect()
    }

    /// Whether `row` carries this module's controller — for the screens' own tests.
    pub(crate) fn passes_vertical_scroll_on(row: &gtk4::ScrolledWindow) -> bool {
        // GTK's own scrolled window has one capture-phase scroll controller; this module adds
        // the second.
        controllers(row).iter().filter(|line| line.starts_with("GtkEventControllerScroll Capture")).count() == 2
    }

    fn sideways_row() -> gtk4::ScrolledWindow {
        gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Automatic)
            .vscrollbar_policy(gtk4::PolicyType::Never)
            .child(&gtk4::Box::new(gtk4::Orientation::Horizontal, 0))
            .build()
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The touchscreen path is the scrolled
    /// window's own touch-only gestures: a row given the wheel handling keeps every controller
    /// it had, in the same order and phase and still touch-only, and gains exactly one scroll
    /// controller (which only ever sees scroll events).
    pub(crate) fn run_rows_keep_every_touch_gesture(_runtime: &tokio::runtime::Runtime) {
        let plain = sideways_row();
        let handled = sideways_row();
        pass_vertical_scroll_to_page(&handled);

        // GTK lists a widget's controllers newest first.
        let before = controllers(&plain);
        let after = controllers(&handled);
        assert!(before.iter().filter(|line| line.ends_with("touch_only=Some(true)")).count() >= 4, "GTK's touch gestures are there to keep: {before:#?}");
        assert_eq!(after.len(), before.len() + 1, "exactly one controller added: {after:#?}");
        assert_eq!(after[0], "GtkEventControllerScroll Capture touch_only=None", "and it's a scroll controller");
        assert_eq!(after[1..], before[..], "every original controller kept, unchanged and in order");
        assert!(passes_vertical_scroll_on(&handled));
        assert!(!passes_vertical_scroll_on(&plain));
    }

    #[test]
    fn a_vertical_wheel_scrolls_the_page_by_gtk_s_own_step() {
        // GTK's wheel step for a 1000 px view is 1000^(2/3) = 100 px per notch.
        let step = route(false, true, 0.0, 1.0, 1000.0).unwrap();
        assert!((step - 100.0).abs() < 1e-9, "{step}");
        assert!((route(false, true, 0.0, -2.0, 1000.0).unwrap() + 200.0).abs() < 1e-9, "up, two notches");
    }

    #[test]
    fn a_touchpad_moves_the_page_by_its_own_pixels() {
        assert_eq!(route(false, false, 1.5, 12.0, 1000.0), Some(12.0));
    }

    #[test]
    fn shift_and_sideways_movement_are_left_to_the_row() {
        assert_eq!(route(true, true, 0.0, 1.0, 1000.0), None, "Shift+wheel scrolls the row");
        assert_eq!(route(false, true, 1.0, 0.0, 1000.0), None, "a tilt wheel scrolls the row");
        assert_eq!(route(false, false, 20.0, 5.0, 1000.0), None, "a mostly sideways swipe scrolls the row");
        assert_eq!(route(false, false, 0.0, 0.0, 1000.0), None);
    }
}
