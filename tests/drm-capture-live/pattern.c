/* SPDX-License-Identifier: MIT */
/* Fullscreen Wayland content for the live compositor capture probe. */
#include <gtk/gtk.h>

static unsigned int shade = 0x49;

static gboolean draw_pattern(GtkWidget *widget, cairo_t *cr, gpointer data)
{
	(void)widget;
	(void)data;
	cairo_set_source_rgb(cr, shade / 255.0, shade / 255.0, shade / 255.0);
	cairo_paint(cr);
	return TRUE;
}

static gboolean change_pattern(gpointer window)
{
	shade = shade == 0x49 ? 0x68 : 0x49;
	gtk_widget_queue_draw(GTK_WIDGET(window));
	return G_SOURCE_CONTINUE;
}

void capture_pattern_client(void)
{
	GtkWidget *window;
	GdkCursor *cursor;

	g_setenv("GDK_BACKEND", "wayland", TRUE);
	gtk_init(NULL, NULL);
	window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
	gtk_window_set_title(GTK_WINDOW(window), "Pronk capture pattern");
	gtk_window_set_decorated(GTK_WINDOW(window), FALSE);
	gtk_widget_set_app_paintable(window, TRUE);
	g_signal_connect(window, "draw", G_CALLBACK(draw_pattern), NULL);
	g_signal_connect(window, "destroy", G_CALLBACK(gtk_main_quit), NULL);
	gtk_window_fullscreen(GTK_WINDOW(window));
	gtk_widget_show_all(window);
	cursor = gdk_cursor_new_for_display(gtk_widget_get_display(window), GDK_BLANK_CURSOR);
	gdk_window_set_cursor(gtk_widget_get_window(window), cursor);
	g_object_unref(cursor);
	g_timeout_add(1000, change_pattern, window);
	gtk_main();
}
