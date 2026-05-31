/*
 * Minimal freestanding shim for <string.h>, used ONLY so bindgen/libclang can
 * parse the SEGGER headers (which pull in <string.h> via the USB_MEMCPY guards)
 * when cross-parsing for a bare-metal target that has no libc headers.
 *
 * This is our own trivial file (not a SEGGER header) and contributes no symbols
 * to the build: bindgen's allowlist drops these, and the real mem/str functions
 * are provided by compiler-builtins and src/segger/libc.rs at link time.
 */
#ifndef SMS_BINDGEN_SHIM_STRING_H
#define SMS_BINDGEN_SHIM_STRING_H
#include <stddef.h>
void  *memcpy (void *, const void *, size_t);
void  *memset (void *, int, size_t);
int    memcmp (const void *, const void *, size_t);
void  *memmove(void *, const void *, size_t);
size_t strlen (const char *);
int    strcmp (const char *, const char *);
int    strncmp(const char *, const char *, size_t);
char  *strcpy (char *, const char *);
char  *strncpy(char *, const char *, size_t);
char  *strcat (char *, const char *);
char  *strchr (const char *, int);
char  *strrchr(const char *, int);
#endif
