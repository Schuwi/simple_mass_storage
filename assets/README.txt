This folder is the root of the USB mass-storage volume.

Everything you drop in here (files and subfolders) is baked into a read-only
FAT image at build time by build.rs (see build/fat_image.rs) and embedded into
the firmware. Rebuild the project after changing these files to update the image.
