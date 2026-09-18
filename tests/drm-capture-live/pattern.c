/* SPDX-License-Identifier: MIT */
/* Fullscreen Wayland content for the live compositor capture probe. */
#include <gtk/gtk.h>

static unsigned int shade = 0x49;
static GList *windows;

static gboolean draw_pattern(GtkWidget *widget, cairo_t *cr, gpointer data)
{
	(void)widget;
	(void)data;
	cairo_set_source_rgb(cr, shade / 255.0, shade / 255.0, shade / 255.0);
	cairo_paint(cr);
	return TRUE;
}

static gboolean change_pattern(gpointer data)
{
	GList *item;
	(void)data;
	shade = shade == 0x49 ? 0x68 : 0x49;
	for (item = windows; item; item = item->next)
		gtk_widget_queue_draw(GTK_WIDGET(item->data));
	return G_SOURCE_CONTINUE;
}

static void add_monitor(GdkDisplay *display, GdkMonitor *monitor, gpointer data)
{
	GtkWidget *window;
	GdkCursor *cursor;
	int index;
	(void)data;

	for (index = 0; index < gdk_display_get_n_monitors(display); index++)
		if (gdk_display_get_monitor(display, index) == monitor)
			break;
	window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
	g_object_set_data(G_OBJECT(monitor), "capture-pattern-window", window);
	windows = g_list_prepend(windows, window);
	gtk_window_set_title(GTK_WINDOW(window), "Pronk capture pattern");
	gtk_window_set_decorated(GTK_WINDOW(window), FALSE);
	gtk_widget_set_app_paintable(window, TRUE);
	g_signal_connect(window, "draw", G_CALLBACK(draw_pattern), NULL);
	gtk_window_fullscreen_on_monitor(GTK_WINDOW(window),
		gdk_display_get_default_screen(display), index);
	gtk_widget_show_all(window);
	cursor = gdk_cursor_new_for_display(gtk_widget_get_display(window), GDK_BLANK_CURSOR);
	gdk_window_set_cursor(gtk_widget_get_window(window), cursor);
	g_object_unref(cursor);
}

static void remove_monitor(GdkDisplay *display, GdkMonitor *monitor, gpointer data)
{
	GtkWidget *window = g_object_get_data(G_OBJECT(monitor), "capture-pattern-window");
	(void)display;
	(void)data;
	if (!window)
		return;
	g_object_set_data(G_OBJECT(monitor), "capture-pattern-window", NULL);
	windows = g_list_remove(windows, window);
	gtk_widget_destroy(window);
}

void capture_pattern_client(void)
{
	GdkDisplay *display;
	int index;

	g_setenv("GDK_BACKEND", "wayland", TRUE);
	gtk_init(NULL, NULL);
	display = gdk_display_get_default();
	g_signal_connect(display, "monitor-added", G_CALLBACK(add_monitor), NULL);
	g_signal_connect(display, "monitor-removed", G_CALLBACK(remove_monitor), NULL);
	for (index = 0; index < gdk_display_get_n_monitors(display); index++)
		add_monitor(display, gdk_display_get_monitor(display, index), NULL);
	g_timeout_add(1000, change_pattern, NULL);
	gtk_main();
}
