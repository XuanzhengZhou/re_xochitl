/*
 * qmldump.so — LD_PRELOAD probe that prints xochitl's live QML object tree.
 *
 * Deliberately avoids symbol interposition: versioned Qt symbols (@Qt_5) do not
 * reliably bind to a foreign definition, so hooking QQmlApplicationEngine::load
 * never fired. Instead we read QCoreApplication's own static `self` pointer
 * (QCoreApplication::instance() is inline in Qt5 and therefore not exported)
 * and let QObject::dumpObjectTree walk everything below it. QQmlEngine parents
 * itself to QCoreApplication when constructed with a null parent, so the whole
 * QML tree hangs off `self`.
 *
 * No Qt headers, no import libraries — every entry point is resolved by its
 * Itanium-mangled name at runtime.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>
#include <string.h>

#define QT_SELF "_ZN16QCoreApplication4selfE"
#define QT_DUMP "_ZN7QObject14dumpObjectTreeEv"
#define QT_FOCUS "_ZN15QGuiApplication11focusWindowEv"

static void *open_any(const char *const *names)
{
    for (int i = 0; names[i]; i++) {
        void *h = dlopen(names[i], RTLD_LAZY | RTLD_GLOBAL);
        if (h)
            return h;
    }
    return NULL;
}

/* How many copies of a given library are mapped right now? */
static int count_maps(const char *needle)
{
    FILE *f = fopen("/proc/self/maps", "r");
    if (!f)
        return -1;
    char line[512];
    int n = 0;
    while (fgets(line, sizeof line, f))
        if (strstr(line, needle))
            n++;
    fclose(f);
    return n;
}

static void *probe(void *arg)
{
    (void)arg;

    typedef void (*dump_t)(void *);
    dump_t dump = NULL;
    void **selfp = NULL;

    /* Poll instead of guessing a startup delay: xochitl may spend a long time
     * before QCoreApplication exists. */
    for (int i = 0; i < 240; i++) {
        usleep(1000000);
        if (!dump)
            dump = (dump_t)dlsym(RTLD_DEFAULT, QT_DUMP);
        if (!selfp)
            selfp = (void **)dlsym(RTLD_DEFAULT, QT_SELF);
        if (selfp && *selfp)
            break;
        if (i % 10 == 0) {
            fprintf(stderr, "[qmldump] heartbeat t=%ds self@%p val=%p dump=%p\n",
                    i, (void *)selfp, selfp ? *selfp : NULL, (void *)dump);
            fflush(stderr);
        }
    }

    if (!dump || !selfp || !*selfp) {
        fprintf(stderr, "[qmldump] gave up: dump=%p self@%p val=%p\n", dump,
                selfp, selfp ? *selfp : NULL);
        fflush(stderr);
        return NULL;
    }

    void *app = *selfp;
    fprintf(stderr, "[qmldump] app=%p (waited for it)\n", app);

    /* xochitl installs its own Qt message handler, so qDebug output (which is
     * what dumpObjectTree uses) never reaches stderr. Restore the default. */
    typedef void *(*install_handler_t)(void *);
    install_handler_t inst = (install_handler_t)dlsym(
        RTLD_DEFAULT,
        "_Z22qInstallMessageHandlerPFv9QtMsgTypeRK18QMessageLogContextRK7QStringE");
    if (inst) {
        void *old = inst(NULL);
        fprintf(stderr, "[qmldump] message handler replaced (old=%p)\n", old);
    } else {
        fprintf(stderr, "[qmldump] qInstallMessageHandler NOT found\n");
    }
    fflush(stderr);

    /* Let the scene graph settle after the app object appears. */
    sleep(20);

    fprintf(stderr, "[qmldump] ===== APP TREE BEGIN =====\n");
    dump(app);
    fprintf(stderr, "[qmldump] ===== APP TREE END =====\n");

    static const char *const gui_names[] = {
        "libQt5Gui.so.5", "/usr/lib/libQt5Gui.so.5", NULL};
    void *gui = open_any(gui_names);
    if (!gui) {
        fflush(stderr);
        return NULL;
    }

    typedef void *(*getter_t)(void);
    getter_t focus = (getter_t)dlsym(gui, QT_FOCUS);
    void *win = focus ? focus() : NULL;
    fprintf(stderr, "[qmldump] focusWindow=%p\n", win);
    if (win) {
        fprintf(stderr, "[qmldump] ===== FOCUS WINDOW TREE BEGIN =====\n");
        dump(win);
        fprintf(stderr, "[qmldump] ===== FOCUS WINDOW TREE END =====\n");
    }

    /* QWindow objects are registered in a global list rather than parented to
     * the application, so walk QGuiApplication::topLevelWindows() by hand.
     * QList<T> is a single pointer to QListData::Data:
     *   { int ref; int alloc; int begin; int end; void *array[]; }
     * with elements at  array + (begin + i) * sizeof(void *). */
    typedef void (*tlw_t)(void *ret);
    tlw_t tlw = (tlw_t)dlsym(gui, "_ZN15QGuiApplication15topLevelWindowsEv");
    if (!tlw) {
        fprintf(stderr, "[qmldump] topLevelWindows not found\n");
        fflush(stderr);
        return NULL;
    }

    void *slot[2] = {0, 0};
    tlw(slot);
    char *d = (char *)slot[0];
    if (!d) {
        fprintf(stderr, "[qmldump] topLevelWindows returned null list\n");
        fflush(stderr);
        return NULL;
    }
    int begin = *(int *)(d + 8);
    int end = *(int *)(d + 12);
    int count = end - begin;
    fprintf(stderr, "[qmldump] topLevelWindows: begin=%d end=%d count=%d\n", begin,
            end, count);
    if (count < 0 || count > 32) {
        fprintf(stderr, "[qmldump] implausible window count, layout guess is wrong\n");
        fflush(stderr);
        return NULL;
    }
    for (int i = 0; i < count; i++) {
        void *w = *(void **)(d + 16 + (begin + i) * (int)sizeof(void *));
        fprintf(stderr, "[qmldump] ===== WINDOW[%d] %p TREE BEGIN =====\n", i, w);
        if (w)
            dump(w);
        fprintf(stderr, "[qmldump] ===== WINDOW[%d] TREE END =====\n", i);
    }
    fflush(stderr);
    return NULL;
}

/* /etc/ld.so.preload hits every dynamically linked program on the system, so
 * bail out immediately unless we are inside the Qt UI process. */
static int is_target(void)
{
    FILE *f = fopen("/proc/self/comm", "r");
    if (!f)
        return 0;
    char buf[64] = {0};
    char *r = fgets(buf, sizeof buf, f);
    fclose(f);
    if (!r)
        return 0;
    return strncmp(buf, "xochitl", 7) == 0;
}

__attribute__((constructor)) static void init(void)
{
    if (!is_target())
        return;

    freopen("/tmp/qmldump.txt", "w", stderr);
    fprintf(stderr, "[qmldump] preloaded pid=%d\n", (int)getpid());
    fflush(stderr);

    pthread_t t;
    if (pthread_create(&t, NULL, probe, NULL) == 0)
        pthread_detach(t);
}
