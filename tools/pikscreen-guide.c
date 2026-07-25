#include <gtk/gtk.h>
#include <gtk4-layer-shell.h>
#include <glib/gstdio.h>
#include <math.h>
#include <pango/pangocairo.h>
#include <stdlib.h>

#define GUIDE_WIDTH 310
#define GUIDE_HEIGHT 190
#define COUNTDOWN_SIZE 220
#define ENTER_MS 520.0
#define FADE_MS 180.0

typedef struct {
    GtkApplication *application;
    GtkWindow *window;
    GtkDrawingArea *area;
    gint64 started_at_us;
    guint duration_ms;
    gint target_x;
    gint target_y;
    gchar *dismiss_path;
    gboolean dismiss_requested;
    gchar *accent;
    gint guide_width;
    gint guide_height;
    gboolean border;
    gboolean countdown;
} Guide;

static double clamp01(double value) {
    return fmax(0.0, fmin(1.0, value));
}

static double ease_out_cubic(double value) {
    value = clamp01(value);
    return 1.0 - pow(1.0 - value, 3.0);
}

static void rounded_rectangle(cairo_t *cr, double x, double y, double width, double height, double radius) {
    const double right = x + width;
    const double bottom = y + height;
    cairo_new_sub_path(cr);
    cairo_arc(cr, right - radius, y + radius, radius, -G_PI_2, 0.0);
    cairo_arc(cr, right - radius, bottom - radius, radius, 0.0, G_PI_2);
    cairo_arc(cr, x + radius, bottom - radius, radius, G_PI_2, G_PI);
    cairo_arc(cr, x + radius, y + radius, radius, G_PI, 3.0 * G_PI_2);
    cairo_close_path(cr);
}

static void draw_countdown(cairo_t *cr, int width, int height, double elapsed_ms) {
    const int digit = 3 - (int)(elapsed_ms / 1000.0);
    if (digit < 1 || digit > 3) {
        return;
    }

    const double phase = fmod(elapsed_ms, 1000.0) / 1000.0;
    const double enter = ease_out_cubic(phase / 0.16);
    const double exit = phase < 0.78 ? 1.0 : 1.0 - ease_out_cubic((phase - 0.78) / 0.22);
    const double alpha = clamp01(enter * exit);
    const double scale = 0.78 + 0.22 * enter;
    const double card_size = 148.0 * scale;
    const double card_x = (width - card_size) / 2.0;
    const double card_y = (height - card_size) / 2.0;

    rounded_rectangle(cr, card_x, card_y, card_size, card_size, 42.0 * scale);
    cairo_set_source_rgba(cr, 0.035, 0.045, 0.065, 0.78 * alpha);
    cairo_fill_preserve(cr);
    cairo_set_source_rgba(cr, 0.45, 0.68, 1.0, 0.72 * alpha);
    cairo_set_line_width(cr, 2.0);
    cairo_stroke(cr);

    gchar text[2] = {(gchar)('0' + digit), '\0'};
    PangoLayout *layout = pango_cairo_create_layout(cr);
    PangoFontDescription *font = pango_font_description_from_string("Sans Bold");
    pango_font_description_set_absolute_size(font, 92.0 * scale * PANGO_SCALE);
    pango_layout_set_font_description(layout, font);
    pango_layout_set_text(layout, text, -1);
    int text_width = 0;
    int text_height = 0;
    pango_layout_get_pixel_size(layout, &text_width, &text_height);
    const double text_x = (width - text_width) / 2.0;
    const double text_y = (height - text_height) / 2.0 - 3.0;

    cairo_move_to(cr, text_x + 2.0, text_y + 4.0);
    cairo_set_source_rgba(cr, 0.0, 0.0, 0.0, 0.42 * alpha);
    pango_cairo_show_layout(cr, layout);
    cairo_move_to(cr, text_x, text_y);
    cairo_set_source_rgba(cr, 0.96, 0.98, 1.0, alpha);
    pango_cairo_show_layout(cr, layout);

    pango_font_description_free(font);
    g_object_unref(layout);
}

static void draw_guide(
    GtkDrawingArea *area,
    cairo_t *cr,
    int width,
    int height,
    gpointer user_data
) {
    Guide *guide = user_data;
    (void)area;
    const double elapsed_ms = (g_get_monotonic_time() - guide->started_at_us) / 1000.0;
    const double total_ms = guide->duration_ms;
    const double enter = ease_out_cubic(elapsed_ms / ENTER_MS);
    const double fade_start = fmax(ENTER_MS, total_ms - FADE_MS);
    const double fade = elapsed_ms <= fade_start
        ? 1.0
        : 1.0 - ease_out_cubic((elapsed_ms - fade_start) / FADE_MS);
    // A mapped Wayland surface may skip its fully transparent first frame, so
    // opacity must be part of every early redraw, not just the exit fade.
    const double alpha = clamp01(enter * fade);
    const double scale = 0.82 + (0.18 * enter);
    const double guide_width = (width - 24.0) * scale;
    const double guide_height = (height - 24.0) * scale;
    const double x = (width - guide_width) / 2.0;
    const double y = (height - guide_height) / 2.0;
    const gboolean orange = g_strcmp0(guide->accent, "orange") == 0;

    cairo_set_operator(cr, CAIRO_OPERATOR_CLEAR);
    cairo_paint(cr);
    cairo_set_operator(cr, CAIRO_OPERATOR_OVER);

    if (guide->countdown) {
        draw_countdown(cr, width, height, elapsed_ms);
        return;
    }

    if (guide->border) {
        const double pulse = 0.5 + 0.5 * sin(elapsed_ms / 700.0);
        for (int ring = 3; ring >= 1; ring--) {
            cairo_rectangle(cr, 4.0 + ring, 4.0 + ring, width - 8.0 - ring * 2.0, height - 8.0 - ring * 2.0);
            cairo_set_source_rgba(cr, 1.0, 0.08, 0.12, (0.05 + pulse * 0.04) * alpha);
            cairo_set_line_width(cr, 3.0 + ring * 3.0);
            cairo_stroke(cr);
        }
        cairo_rectangle(cr, 4.0, 4.0, width - 8.0, height - 8.0);
        cairo_set_source_rgba(cr, 1.0, 0.16, 0.20, (0.55 + pulse * 0.25) * alpha);
        cairo_set_line_width(cr, 2.0);
        cairo_stroke(cr);
        return;
    }

    rounded_rectangle(cr, x, y, guide_width, guide_height, 18.0);
    cairo_set_source_rgba(
        cr,
        orange ? 1.0 : 0.08,
        orange ? 0.34 : 0.48,
        orange ? 0.04 : 1.0,
        0.08 * alpha
    );
    cairo_fill_preserve(cr);
    cairo_set_source_rgba(
        cr,
        orange ? 1.0 : 0.25,
        orange ? 0.58 : 0.68,
        orange ? 0.16 : 1.0,
        0.95 * alpha
    );
    cairo_set_line_width(cr, 2.5);
    cairo_stroke(cr);
}

static gboolean tick_guide(gpointer user_data) {
    Guide *guide = user_data;
    const double elapsed_ms = (g_get_monotonic_time() - guide->started_at_us) / 1000.0;
    if (!guide->dismiss_requested && guide->dismiss_path != NULL && g_file_test(guide->dismiss_path, G_FILE_TEST_EXISTS)) {
        guide->dismiss_requested = TRUE;
        guide->duration_ms = (guint)ceil(fmax(elapsed_ms + FADE_MS, ENTER_MS + FADE_MS));
    }
    const double enter = ease_out_cubic(elapsed_ms / ENTER_MS);
    const double fade_start = fmax(ENTER_MS, guide->duration_ms - FADE_MS);
    const double fade = elapsed_ms <= fade_start
        ? 1.0
        : 1.0 - ease_out_cubic((elapsed_ms - fade_start) / FADE_MS);
    // Window opacity updates the Wayland surface's compositor-visible alpha.
    // Cairo-only alpha can be coalesced with the initial map commit.
    gtk_widget_set_opacity(
        GTK_WIDGET(guide->window),
        guide->countdown ? 1.0 : fmax(0.01, enter * clamp01(fade))
    );
    gtk_widget_queue_draw(GTK_WIDGET(guide->area));
    if (elapsed_ms >= guide->duration_ms) {
        gtk_window_destroy(guide->window);
        g_application_quit(G_APPLICATION(guide->application));
        return G_SOURCE_REMOVE;
    }
    return G_SOURCE_CONTINUE;
}

static GdkMonitor *monitor_for_target(gint x, gint y) {
    GdkDisplay *display = gdk_display_get_default();
    GListModel *monitors = gdk_display_get_monitors(display);
    const guint count = g_list_model_get_n_items(monitors);
    GdkMonitor *fallback = NULL;

    for (guint index = 0; index < count; index++) {
        GdkMonitor *monitor = g_list_model_get_item(monitors, index);
        GdkRectangle geometry;
        gdk_monitor_get_geometry(monitor, &geometry);
        if (fallback == NULL) {
            fallback = g_object_ref(monitor);
        }
        if (x >= geometry.x && x < geometry.x + geometry.width && y >= geometry.y && y < geometry.y + geometry.height) {
            if (fallback != NULL) {
                g_object_unref(fallback);
            }
            return monitor;
        }
        g_object_unref(monitor);
    }
    return fallback;
}

static void activate(GtkApplication *application, gpointer user_data) {
    Guide *guide = user_data;
    GdkMonitor *monitor = monitor_for_target(guide->target_x, guide->target_y);
    GdkRectangle geometry = {0};
    if (monitor != NULL) {
        gdk_monitor_get_geometry(monitor, &geometry);
    }

    guide->window = GTK_WINDOW(gtk_application_window_new(application));
    GtkCssProvider *css = gtk_css_provider_new();
    gtk_css_provider_load_from_string(css, "window { background-color: transparent; }");
    gtk_style_context_add_provider_for_display(
        gdk_display_get_default(),
        GTK_STYLE_PROVIDER(css),
        GTK_STYLE_PROVIDER_PRIORITY_APPLICATION
    );
    g_object_unref(css);
    gtk_window_set_decorated(guide->window, FALSE);
    gtk_window_set_resizable(guide->window, FALSE);
    gtk_window_set_default_size(
        guide->window,
        guide->border ? geometry.width : (guide->countdown ? COUNTDOWN_SIZE : guide->guide_width),
        guide->border ? geometry.height : (guide->countdown ? COUNTDOWN_SIZE : guide->guide_height)
    );
    gtk_widget_set_opacity(GTK_WIDGET(guide->window), 0.01);

    gtk_layer_init_for_window(guide->window);
    gtk_layer_set_namespace(guide->window, "pikscreen-guide");
    gtk_layer_set_layer(guide->window, GTK_LAYER_SHELL_LAYER_OVERLAY);
    gtk_layer_set_exclusive_zone(guide->window, 0);
    gtk_layer_set_keyboard_mode(guide->window, GTK_LAYER_SHELL_KEYBOARD_MODE_NONE);
    gtk_layer_set_anchor(guide->window, GTK_LAYER_SHELL_EDGE_LEFT, TRUE);
    gtk_layer_set_anchor(guide->window, GTK_LAYER_SHELL_EDGE_TOP, TRUE);
    if (guide->border) {
        gtk_layer_set_anchor(guide->window, GTK_LAYER_SHELL_EDGE_RIGHT, TRUE);
        gtk_layer_set_anchor(guide->window, GTK_LAYER_SHELL_EDGE_BOTTOM, TRUE);
    }
    if (monitor != NULL) {
        gtk_layer_set_monitor(guide->window, monitor);
        g_object_unref(monitor);
    }

    if (!guide->border) {
        const gint surface_width = guide->countdown ? COUNTDOWN_SIZE : guide->guide_width;
        const gint surface_height = guide->countdown ? COUNTDOWN_SIZE : guide->guide_height;
        const gint local_x = guide->countdown
            ? geometry.width / 2
            : guide->target_x - geometry.x;
        const gint local_y = guide->countdown
            ? geometry.height / 2
            : guide->target_y - geometry.y;
        gtk_layer_set_margin(
            guide->window,
            GTK_LAYER_SHELL_EDGE_LEFT,
            CLAMP(local_x - (surface_width / 2), 0, MAX(0, geometry.width - surface_width))
        );
        gtk_layer_set_margin(
            guide->window,
            GTK_LAYER_SHELL_EDGE_TOP,
            CLAMP(local_y - (surface_height / 2), 0, MAX(0, geometry.height - surface_height))
        );
    }

    guide->area = GTK_DRAWING_AREA(gtk_drawing_area_new());
    gtk_drawing_area_set_draw_func(guide->area, draw_guide, guide, NULL);
    gtk_widget_set_size_request(
        GTK_WIDGET(guide->area),
        guide->border ? geometry.width : (guide->countdown ? COUNTDOWN_SIZE : guide->guide_width),
        guide->border ? geometry.height : (guide->countdown ? COUNTDOWN_SIZE : guide->guide_height)
    );
    gtk_window_set_child(guide->window, GTK_WIDGET(guide->area));
    // The border is a visual-only layer. An overlay surface otherwise owns the
    // full-screen Wayland input region and prevents every click below it.
    gtk_widget_set_can_target(GTK_WIDGET(guide->window), FALSE);
    gtk_widget_set_can_target(GTK_WIDGET(guide->area), FALSE);

    guide->started_at_us = g_get_monotonic_time();
    gtk_window_present(guide->window);
    GdkSurface *surface = gtk_native_get_surface(GTK_NATIVE(guide->window));
    if (surface != NULL) {
        cairo_region_t *empty_region = cairo_region_create();
        gdk_surface_set_input_region(surface, empty_region);
        cairo_region_destroy(empty_region);
    }
    g_timeout_add(16, tick_guide, guide);
}

int main(int argc, char **argv) {
    Guide guide = {
        .duration_ms = 3000,
        .target_x = 0,
        .target_y = 0,
        .guide_width = GUIDE_WIDTH,
        .guide_height = GUIDE_HEIGHT,
    };
    GOptionEntry entries[] = {
        {"x", 'x', 0, G_OPTION_ARG_INT, &guide.target_x, "Global cursor x coordinate", "PX"},
        {"y", 'y', 0, G_OPTION_ARG_INT, &guide.target_y, "Global cursor y coordinate", "PX"},
        {"width", 0, 0, G_OPTION_ARG_INT, &guide.guide_width, "Guide width", "PX"},
        {"height", 0, 0, G_OPTION_ARG_INT, &guide.guide_height, "Guide height", "PX"},
        {"duration-ms", 'd', 0, G_OPTION_ARG_INT, &guide.duration_ms, "Guide duration", "MS"},
        {"dismiss-file", 0, 0, G_OPTION_ARG_FILENAME, &guide.dismiss_path, "Close guide when this file appears", "PATH"},
        {"accent", 0, 0, G_OPTION_ARG_STRING, &guide.accent, "Guide accent color", "COLOR"},
        {"border", 0, 0, G_OPTION_ARG_NONE, &guide.border, "Show the recording border", NULL},
        {"countdown", 0, 0, G_OPTION_ARG_NONE, &guide.countdown, "Show a three-second recording countdown", NULL},
        {NULL}
    };
    GOptionContext *context = g_option_context_new(NULL);
    g_option_context_add_main_entries(context, entries, NULL);
    GError *error = NULL;
    if (!g_option_context_parse(context, &argc, &argv, &error)) {
        g_printerr("%s\n", error->message);
        g_error_free(error);
        g_option_context_free(context);
        return EXIT_FAILURE;
    }
    g_option_context_free(context);

    guide.guide_width = MAX(1, guide.guide_width);
    guide.guide_height = MAX(1, guide.guide_height);

    gtk_disable_portals();
    guide.application = gtk_application_new("dev.pikyusufaslan.pikscreen.guide", G_APPLICATION_NON_UNIQUE);
    g_signal_connect(guide.application, "activate", G_CALLBACK(activate), &guide);
    const int status = g_application_run(G_APPLICATION(guide.application), argc, argv);
    g_object_unref(guide.application);
    if (guide.dismiss_path != NULL) {
        g_remove(guide.dismiss_path);
        g_free(guide.dismiss_path);
    }
    g_free(guide.accent);
    return status;
}
