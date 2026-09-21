/* SPDX-License-Identifier: MIT */
/* Fullscreen Wayland content for the live compositor capture probe. */
#include <gtk/gtk.h>

struct pattern_window {
	GdkMonitor *monitor;
	GtkWindow *window;
};

static unsigned int shade = 0x49;
static GList *windows;

static void draw_pattern(GtkDrawingArea *area, cairo_t *cr, int width,
	int height, gpointer data)
{
	(void)area;
	(void)width;
	(void)height;
	(void)data;
	cairo_set_source_rgb(cr, shade / 255.0, shade / 255.0, shade / 255.0);
	cairo_paint(cr);
}

static gboolean change_pattern(gpointer data)
{
	GList *item;
	(void)data;
	shade = shade == 0x49 ? 0x68 : 0x49;
	for (item = windows; item; item = item->next) {
		struct pattern_window *pattern = item->data;
		GtkWidget *child = gtk_window_get_child(pattern->window);

		gtk_widget_queue_draw(child);
	}
	return G_SOURCE_CONTINUE;
}

static struct pattern_window *find_monitor(GdkMonitor *monitor)
{
	GList *item;

	for (item = windows; item; item = item->next) {
		struct pattern_window *pattern = item->data;

		if (pattern->monitor == monitor)
			return pattern;
	}
	return NULL;
}

static gboolean monitor_is_present(GListModel *monitors, GdkMonitor *monitor)
{
	guint index;

	for (index = 0; index < g_list_model_get_n_items(monitors); index++) {
		GdkMonitor *candidate = g_list_model_get_item(monitors, index);
		gboolean matches = candidate == monitor;

		g_object_unref(candidate);
		if (matches)
			return TRUE;
	}
	return FALSE;
}

static void add_monitor(GtkApplication *application, GdkMonitor *monitor)
{
	struct pattern_window *pattern = g_new0(struct pattern_window, 1);
	GtkWidget *area = gtk_drawing_area_new();

	pattern->monitor = g_object_ref(monitor);
	pattern->window = GTK_WINDOW(gtk_application_window_new(application));
	gtk_window_set_title(pattern->window, "Pronk capture pattern");
	gtk_window_set_decorated(pattern->window, FALSE);
	gtk_drawing_area_set_draw_func(GTK_DRAWING_AREA(area), draw_pattern,
		NULL, NULL);
	gtk_window_set_child(pattern->window, area);
	gtk_widget_set_cursor_from_name(GTK_WIDGET(pattern->window), "none");
	gtk_window_fullscreen_on_monitor(pattern->window, monitor);
	gtk_window_present(pattern->window);
	windows = g_list_prepend(windows, pattern);
}

static void remove_monitor(struct pattern_window *pattern)
{
	windows = g_list_remove(windows, pattern);
	gtk_window_destroy(pattern->window);
	g_object_unref(pattern->monitor);
	g_free(pattern);
}

static void sync_monitors(GtkApplication *application, GListModel *monitors)
{
	GList *item = windows;
	guint index;

	while (item) {
		GList *next = item->next;
		struct pattern_window *pattern = item->data;

		if (!monitor_is_present(monitors, pattern->monitor))
			remove_monitor(pattern);
		item = next;
	}

	for (index = 0; index < g_list_model_get_n_items(monitors); index++) {
		GdkMonitor *monitor = g_list_model_get_item(monitors, index);

		if (!find_monitor(monitor))
			add_monitor(application, monitor);
		g_object_unref(monitor);
	}
}

static void monitors_changed(GListModel *monitors, guint position,
	guint removed, guint added, gpointer data)
{
	(void)position;
	(void)removed;
	(void)added;
	sync_monitors(GTK_APPLICATION(data), monitors);
}

static void activate(GtkApplication *application, gpointer data)
{
	GdkDisplay *display = gdk_display_get_default();
	GListModel *monitors;
	(void)data;

	g_assert(display != NULL);
	monitors = gdk_display_get_monitors(display);
	g_signal_connect(monitors, "items-changed", G_CALLBACK(monitors_changed),
		application);
	sync_monitors(application, monitors);
	g_timeout_add(1000, change_pattern, NULL);
}

void capture_pattern_client(void)
{
	GtkApplication *application;

	g_setenv("GDK_BACKEND", "wayland", TRUE);
	application = gtk_application_new("org.pronkproject.CapturePattern",
		G_APPLICATION_NON_UNIQUE);
	g_signal_connect(application, "activate", G_CALLBACK(activate), NULL);
	g_application_run(G_APPLICATION(application), 0, NULL);
	g_object_unref(application);
}
