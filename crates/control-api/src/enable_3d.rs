//! The words both ends of *Add 3D graphics* share: `entangled enable-3d`
//! writes the disc, and the manager tells the person what to do with it. One
//! spelling, so the instruction on screen always names the disc that exists.

/// The volume label of the disc `entangled enable-3d` attaches: `LABEL=` on
/// Linux, the drive's name in Windows' File Explorer.
pub const DISC_LABEL: &str = "ENTANGLED3D";

/// The script on a Linux guest's disc.
pub const LINUX_SCRIPT: &str = "enable-3d.sh";

/// The line a person pastes into a Linux guest's terminal: mount the disc,
/// run the script as root, unmount it again.
pub const LINUX_COMMAND: &str =
    "sudo mount -o ro LABEL=ENTANGLED3D /mnt && sudo sh /mnt/enable-3d.sh; sudo umount /mnt";

/// The script on a Windows guest's disc.
pub const WINDOWS_SCRIPT: &str = "Install 3D.cmd";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_names_the_disc_and_the_script() {
        assert!(LINUX_COMMAND.contains(&format!("LABEL={DISC_LABEL}")));
        assert!(LINUX_COMMAND.contains(LINUX_SCRIPT));
        // ISO 9660 labels are at most 16 upper-case characters.
        assert!(DISC_LABEL.len() <= 16 && DISC_LABEL == DISC_LABEL.to_uppercase());
    }
}
