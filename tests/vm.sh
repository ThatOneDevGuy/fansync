#!/bin/bash
# Boots the running kernel in a throwaway QEMU VM, with this machine's
# filesystem shared read-only over 9p, and runs COMMAND there as real root
# (from the fansync directory). Works without KVM. Example:
#   tests/vm.sh tests/vm-tests.sh
# Needs qemu-system-x86, busybox-static and zstd, and a readable
# /boot/vmlinuz-$KERNEL with its modules; KERNEL defaults to the running
# kernel if it's installed, else the newest installed one.
set -euo pipefail
cmd=${1:?usage: tests/vm.sh COMMAND}
here=$(cd "$(dirname "$0")/.." && pwd)
K=${KERNEL:-$(uname -r)}
if [[ ! -e /boot/vmlinuz-$K || ! -d /lib/modules/$K ]]; then
  K=$(ls /lib/modules | sort -V | while read -r k; do [[ -e /boot/vmlinuz-$k ]] && echo "$k"; done | tail -1)
fi
V=$(mktemp -d)
trap 'rm -rf "$V"' EXIT
mkdir -p "$V"/initrd/{bin,mod,dev,proc,sys,newroot}
cp "$(command -v busybox)" "$V/initrd/bin/"
for m in fs/netfs/netfs net/9p/9pnet net/9p/9pnet_virtio fs/9p/9p; do
  f=/lib/modules/$K/kernel/$m.ko
  if [[ -e $f.zst ]]; then unzstd -q -f "$f.zst" -o "$V/initrd/mod/${m##*/}.ko"; elif [[ -e $f ]]; then cp "$f" "$V/initrd/mod/"; fi
done
cat > "$V/initrd/init" <<INIT
#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t devtmpfs dev /dev; mount -t proc proc /proc; mount -t sysfs sys /sys
for m in netfs 9pnet 9pnet_virtio 9p; do [ -e /mod/\$m.ko ] && insmod /mod/\$m.ko; done
mount -t 9p -o trans=virtio,version=9p2000.L,ro,msize=1048576 hostroot /newroot
mount -t devtmpfs dev /newroot/dev; mount -t proc proc /newroot/proc; mount -t sysfs sys /newroot/sys
ln -sf /proc/self/fd /newroot/dev/fd; for i in 0 1 2; do ln -sf /proc/self/fd/\$i /newroot/dev/std\$([ \$i = 0 ] && echo in || ([ \$i = 1 ] && echo out || echo err)); done
mkdir -p /newroot/dev/pts /newroot/dev/shm; mount -t devpts pts /newroot/dev/pts; mount -t tmpfs shm /newroot/dev/shm
mount -t tmpfs -o size=6g tmp /newroot/tmp; mount -t tmpfs run /newroot/run
chroot /newroot /bin/bash -c "export PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/root; cd '$here' && $cmd"
poweroff -f
INIT
chmod +x "$V/initrd/init"
(cd "$V/initrd" && find . | busybox cpio -o -H newc 2>/dev/null | gzip -1 > "$V/initrd.gz")
accel=tcg,thread=multi
[[ -w /dev/kvm ]] && accel=kvm
qemu-system-x86_64 -m 8G -smp "$(nproc)" -accel "$accel" -cpu max \
  -kernel "/boot/vmlinuz-$K" -initrd "$V/initrd.gz" \
  -append "console=ttyS0 quiet loglevel=3 panic=-1" \
  -nographic -no-reboot -monitor none -serial stdio \
  -virtfs local,path=/,mount_tag=hostroot,security_model=none,readonly=on
