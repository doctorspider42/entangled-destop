# Unattended Fedora Workstation installation for Entangled Desktop.
#
# Delivered to Anaconda as `ks.cfg` on a small ISO9660 volume labelled OEMDRV
# (apps/entangled/src/seed.rs), which the installer VM sees as its third
# virtio-blk device, and named explicitly on the kernel command line as
# `inst.ks=hd:LABEL=OEMDRV:/ks.cfg`.
#
# ## Why this shape
#
# * **Kickstart, not preseed and not autoinstall.** Fedora's installer is
#   Anaconda and its automation language is kickstart; nothing about the Debian
#   or Ubuntu mechanisms carries over except the *delivery* — a labelled
#   read-only volume, so the verified ISO is never repacked.
# * **The Everything netinst image, not the Workstation Live one.** The Live
#   ISO's initramfs carries no anaconda dracut module at all (no
#   `parse-kickstart`, no `fetch-kickstart-disk`, no OEMDRV rule), so it has no
#   way to *find* a kickstart; and on Live media `%packages` is ignored, because
#   the install is a copy of the live filesystem. The netinst installer honours
#   both, which is why the package group below is the real thing.
# * `@^workstation-product-environment` is the same environment group the
#   Workstation edition installs, so what lands on the disk is Fedora
#   Workstation — GNOME, GDM, the lot — assembled from the network rather than
#   copied from a squashfs.
#
# The @PLACEHOLDER@ values are substituted by `entangled install fedora`
# (apps/entangled/src/install_fedora.rs).

# Text mode. The installer VM has a virtio-gpu window, but the *serial* console
# is the channel `entangled install` reads to know what happened, and Anaconda's
# text UI is the one that speaks it. A graphical run would also want a display
# server inside the installer environment for no gain here.
text

# Power off rather than reboot when the install finishes: an orderly poweroff
# writes S5 to the ACPI PM1a control register, which this machine latches
# (machine_x86::acpi::pm) and reports as RunOutcome::Shutdown. That is how
# `entangled install` knows the installation is over — a reboot would instead
# start the installer again from the still-attached ISO.
poweroff

# --- localisation -----------------------------------------------------------
keyboard --xlayouts='us'
lang en_US.UTF-8
timezone Etc/UTC --utc

# --- network ----------------------------------------------------------------
# Static, not DHCP — the same choice the Debian preseed makes, for the same
# reason: it removes a boot-time negotiation from the middle of an unattended
# install, and the numbers come from the VMM's own network configuration so the
# two cannot drift apart. A netinst install *must* have a network; every package
# comes over it.
network --bootproto=static --ip=@IP@ --netmask=@NETMASK@ --gateway=@GATEWAY@ --nameserver=@DNS@ --device=link --activate --hostname=@HOSTNAME@

# --- storage ----------------------------------------------------------------
# /dev/vda only. The ISO is /dev/vdb and the kickstart volume /dev/vdc, and an
# installer that wiped either would be destroying read-only media it is standing
# on. `--only-use` is the guard; `clearpart --drives` repeats it, because two
# statements agreeing is what makes this safe to run unattended.
ignoredisk --only-use=vda
clearpart --all --initlabel --drives=vda
# Fedora Workstation's own default layout: an ESP, an ext4 /boot, and btrfs
# subvolumes for / and /home. Whether the ESP appears at all is decided by the
# firmware the installer booted under, which is why the installer VM boots
# through EDK2 and not through a direct kernel load.
autopart --type=btrfs
bootloader --boot-drive=vda --append="console=tty0 console=ttyS0,115200n8"

# --- accounts ---------------------------------------------------------------
rootpw --lock
# SHA-512 crypt of "entangled" (openssl passwd -6 -salt entangled0seed), the
# same demo credential the Ubuntu profile uses. Deliberately written down rather
# than hidden: this profile is for automated boot tests on throwaway VMs.
user --name=entangled --gecos="Entangled" --groups=wheel --iscrypted --password=$6$entangled0seed$bT7xSYZVhNBFpEbtY2oAblEwSEksNuxBzAR9uMlGvF/Am7Xk57rlH.Sr6eVuvZM4ePjO968h1NJrBI8Usw14m.

# gnome-initial-setup would otherwise take over the first boot and there would
# be no way to see a desktop unattended.
firstboot --disable

%packages
@^workstation-product-environment
%end

%post --log=/root/entangled-post.log
# The installed system must talk on ttyS0: that is the only channel through
# which `entangled run` can show that the *installed* Fedora booted. The kernel
# argument came from `bootloader --append` above; this adds the getty, and puts
# GRUB's own menu on the serial line too so a failure to load the kernel is
# visible instead of silent.
systemctl enable serial-getty@ttyS0.service
grub2-editenv - unset menu_auto_hide || true
cat >> /etc/default/grub <<'EOF'
GRUB_TERMINAL="serial console"
GRUB_SERIAL_COMMAND="serial --unit=0 --speed=115200"
GRUB_TIMEOUT=3
GRUB_TIMEOUT_STYLE=menu
EOF
grub2-mkconfig -o /etc/grub2-efi.cfg || grub2-mkconfig -o /boot/grub2/grub.cfg || true

# Log in to GNOME without a password, so a boot test reaches a desktop rather
# than a greeter it cannot type into. Same reason as the Debian Weston profile's
# autologin drop-in.
# GKeyFile refuses a file with two [daemon] groups, so the keys go *into* the
# section Fedora already ships rather than after it.
mkdir -p /etc/gdm
if grep -q '^\[daemon\]' /etc/gdm/custom.conf 2>/dev/null; then
    sed -i '0,/^\[daemon\]/s//[daemon]\nAutomaticLoginEnable=True\nAutomaticLogin=entangled/' \
        /etc/gdm/custom.conf
else
    printf '[daemon]\nAutomaticLoginEnable=True\nAutomaticLogin=entangled\n' \
        >> /etc/gdm/custom.conf
fi
%end
