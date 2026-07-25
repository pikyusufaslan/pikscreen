import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'dev.pikyusufaslan.PikScreen.Guides';
const OBJECT_PATH = '/dev/pikyusufaslan/PikScreen/Guides';
const INTERFACE_XML = `
<node>
  <interface name="dev.pikyusufaslan.PikScreen.Guides">
    <method name="Ping">
      <arg type="s" name="version" direction="out"/>
    </method>
    <method name="GetPointer">
      <arg type="i" name="x" direction="out"/>
      <arg type="i" name="y" direction="out"/>
    </method>
    <method name="ShowGuide">
      <arg type="u" name="id" direction="in"/>
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
      <arg type="s" name="accent" direction="in"/>
      <arg type="u" name="duration_ms" direction="in"/>
    </method>
    <method name="Dismiss">
      <arg type="u" name="id" direction="in"/>
    </method>
    <method name="ShowCountdown">
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
      <arg type="u" name="duration_ms" direction="in"/>
    </method>
    <method name="ShowBorder">
      <arg type="u" name="id" direction="in"/>
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
    </method>
  </interface>
</node>`;

const GUIDE_WIDTH = 448;
const GUIDE_HEIGHT = 292;
const GUIDE_FADE_MS = 180;

function clamp(value, lower, upper) {
    return Math.max(lower, Math.min(value, upper));
}

class GuideService {
    constructor() {
        this._actors = new Map();
        this._dbus = Gio.DBusExportedObject.wrapJSObject(INTERFACE_XML, this);
        this._dbus.export(Gio.DBus.session, OBJECT_PATH);
        this._nameId = Gio.bus_own_name_on_connection(
            Gio.DBus.session,
            BUS_NAME,
            Gio.BusNameOwnerFlags.NONE,
            null,
            null,
        );
    }

    destroy() {
        for (const id of [...this._actors.keys()])
            this._dismiss(id, false);
        this._dbus.unexport();
        Gio.bus_unown_name(this._nameId);
    }

    Ping() {
        return '1';
    }

    GetPointer() {
        const [x, y] = global.get_pointer();
        return [x, y];
    }

    ShowGuide(id, x, y, accent, durationMs) {
        this._dismiss(id, false);
        const geometry = this._monitorGeometryAt(x, y);
        const actor = new St.Widget({
            style_class: accent === 'orange'
                ? 'pikscreen-guide pikscreen-guide-orange'
                : 'pikscreen-guide',
            reactive: false,
            can_focus: false,
            width: GUIDE_WIDTH,
            height: GUIDE_HEIGHT,
            opacity: 0,
        });
        actor.set_position(
            clamp(Math.round(x - GUIDE_WIDTH / 2), geometry.x, geometry.x + geometry.width - GUIDE_WIDTH),
            clamp(Math.round(y - GUIDE_HEIGHT / 2), geometry.y, geometry.y + geometry.height - GUIDE_HEIGHT),
        );
        this._addActor(id, actor, durationMs);
    }

    Dismiss(id) {
        this._dismiss(id, true);
    }

    ShowCountdown(x, y, durationMs) {
        const id = 0;
        this._dismiss(id, false);
        const geometry = this._monitorGeometryAt(x, y);
        const actor = new St.Label({
            style_class: 'pikscreen-countdown',
            reactive: false,
            can_focus: false,
            text: String(Math.max(1, Math.ceil(durationMs / 1000))),
            width: 180,
            height: 180,
            opacity: 0,
        });
        Main.layoutManager.addTopChrome(actor, {
            trackFullscreen: false,
        });
        actor.set_position(
            geometry.x + Math.round((geometry.width - actor.width) / 2),
            geometry.y + Math.round((geometry.height - actor.height) / 2),
        );
        actor.ease({
            opacity: 255,
            duration: GUIDE_FADE_MS,
            mode: Clutter.AnimationMode.EASE_OUT_QUAD,
        });

        const started = GLib.get_monotonic_time();
        const sourceId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 50, () => {
            const elapsedMs = Math.floor((GLib.get_monotonic_time() - started) / 1000);
            const remaining = Math.max(0, Math.ceil((durationMs - elapsedMs) / 1000));
            if (remaining === 0) {
                this._dismiss(id, true);
                return GLib.SOURCE_REMOVE;
            }
            actor.set_text(String(remaining));
            return GLib.SOURCE_CONTINUE;
        });
        this._actors.set(id, {actor, sourceId});
    }

    ShowBorder(id, x, y) {
        this._dismiss(id, false);
        const geometry = this._monitorGeometryAt(x, y);
        const actor = new St.Widget({
            style_class: 'pikscreen-recording-border',
            reactive: false,
            can_focus: false,
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
            opacity: 0,
        });
        Main.layoutManager.addTopChrome(actor, {
            trackFullscreen: false,
        });
        actor.ease({
            opacity: 255,
            duration: GUIDE_FADE_MS,
            mode: Clutter.AnimationMode.EASE_OUT_QUAD,
        });
        this._actors.set(id, {actor, sourceId: 0});
    }

    _monitorGeometryAt(x, y) {
        for (let index = 0; index < global.display.get_n_monitors(); index++) {
            const geometry = global.display.get_monitor_geometry(index);
            if (x >= geometry.x && x < geometry.x + geometry.width &&
                y >= geometry.y && y < geometry.y + geometry.height)
                return geometry;
        }
        return global.display.get_monitor_geometry(global.display.get_primary_monitor());
    }

    _addActor(id, actor, durationMs) {
        Main.layoutManager.addTopChrome(actor, {
            trackFullscreen: false,
        });
        actor.ease({
            opacity: 255,
            duration: GUIDE_FADE_MS,
            mode: Clutter.AnimationMode.EASE_OUT_QUAD,
        });
        const delay = Math.max(GUIDE_FADE_MS, durationMs - GUIDE_FADE_MS);
        const sourceId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, delay, () => {
            this._dismiss(id, true);
            return GLib.SOURCE_REMOVE;
        });
        this._actors.set(id, {actor, sourceId});
    }

    _dismiss(id, fade) {
        const entry = this._actors.get(id);
        if (!entry)
            return;
        this._actors.delete(id);
        if (entry.sourceId)
            GLib.source_remove(entry.sourceId);
        const destroy = () => {
            Main.layoutManager.removeChrome(entry.actor);
            entry.actor.destroy();
        };
        if (!fade) {
            destroy();
            return;
        }
        entry.actor.ease({
            opacity: 0,
            duration: GUIDE_FADE_MS,
            mode: Clutter.AnimationMode.EASE_OUT_QUAD,
            onComplete: destroy,
        });
    }
}

export default class PikScreenGuideExtension extends Extension {
    enable() {
        this._service = new GuideService();
    }

    disable() {
        this._service?.destroy();
        this._service = null;
    }
}
