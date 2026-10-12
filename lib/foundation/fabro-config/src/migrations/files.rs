#![expect(
    clippy::disallowed_methods,
    reason = "temporary startup config migrations create files synchronously before config is loaded"
)]

use std::fs::{File, OpenOptions, Permissions};
use std::io;
use std::path::Path;

/// Create a migration destination without granting more Unix mode permissions
/// than the source. Set the mode at creation so even an empty file is
/// protected; the process umask may further restrict it. Special mode bits are
/// not copied.
pub(super) fn create_new_file(path: &Path, permissions: &Permissions) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        options.mode(permissions.mode() & 0o777);
    }
    #[cfg(not(unix))]
    let _ = permissions;

    options.open(path)
}
