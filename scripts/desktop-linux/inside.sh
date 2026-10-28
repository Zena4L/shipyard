# Inside the desktop image, as root (scripts/desktop-linux.sh): the
# repository copied (its mount is read-only, and npm writes beside the
# page), `shipyard` and the app built, a rootless `tester` made ready, and
# the app driven as it. Nothing here serves tester's API: the app must start
# it (F190).
set -euo pipefail
mount -t tmpfs -o mode=1777 tmpfs /tmp
mount --make-rshared /
# The image's own toolchain, as it is: the repository's rust-toolchain.toml
# names components and Linux targets these builds don't use, and rustup
# would download them first (from a network that drops connections).
RUSTUP_TOOLCHAIN=$(cd / && rustup default | cut -d' ' -f1)
export RUSTUP_TOOLCHAIN

echo "== the tree, copied"
mkdir -p /work
tar -C /src --exclude=./target --exclude=./desktop/target --exclude=node_modules \
    --exclude=./desktop/ui/dist --exclude=./docs/book -cf - . | tar -C /work -xf -

echo "== crates: the Mac's and the test container's, copied in"
# A crate's archive and its index entry are the same files on every
# platform: those already on this machine (the Mac's builds, the Linux test
# passes') are taken, never fetched again, so only what neither has comes
# over the network.
for seed in /seed/mac /seed/linux/registry; do
  for kind in cache index; do
    [ -d "$seed/$kind" ] || continue
    mkdir -p /usr/local/cargo/registry/$kind
    cp -rn "$seed/$kind/." /usr/local/cargo/registry/$kind/ 2>/dev/null || true
  done
done
ls /usr/local/cargo/registry/cache/*/ | wc -l | sed 's/$/ crates at hand/'

echo "== shipyard"
(cd /work && cargo build --locked -p shipyard-cli --bin shipyard 2>&1 | tail -3)
# In a directory of root's alone: the app runs only a `shipyard` no other
# user could replace (`place::trusted`), and the image's /usr/local/bin
# belongs to the uid Node's tarball carried.
install -d -m 755 -o root -g root /opt/shipyard/bin
install -m 755 /target/debug/shipyard /opt/shipyard/bin/shipyard

echo "== the app"
(cd /work/desktop/ui && npm ci --no-audit --no-fund --ignore-scripts >/dev/null && npm run build >/dev/null)
(cd /work/desktop/src-tauri && cargo build --locked --features tauri/custom-protocol 2>&1 | tail -3)
install -m 755 /target/debug/shipyard-desktop /opt/shipyard/bin/shipyard-desktop
if [ "${BUILD_ONLY:-}" = 1 ]; then
  mkdir -p /out/bin
  install -m 755 /opt/shipyard/bin/shipyard /opt/shipyard/bin/shipyard-desktop /out/bin/
  echo "built: target/desktop-linux/bin/"
  exit 0
fi

echo "== tester: a delegated cgroup, a runtime directory, an image"
cg=/sys/fs/cgroup
if [ "$(cat /proc/self/cgroup)" = '0::/' ]; then
  mkdir -p $cg/init
  for _ in 1 2 3 4 5; do
    [ -z "$(cat $cg/cgroup.procs)" ] && break
    while read -r pid; do echo "$pid" >$cg/init/cgroup.procs 2>/dev/null || true; done <$cg/cgroup.procs
  done
  if [ -n "$(cat $cg/cgroup.procs)" ]; then
    echo "processes left in the root cgroup: controllers can't be delegated" >&2
    exit 1
  fi
  mkdir -p $cg/user.slice/tester/session
  for c in $(cat $cg/cgroup.controllers); do echo "+$c" >$cg/cgroup.subtree_control; done
  echo '+cpu +memory +pids' >$cg/user.slice/cgroup.subtree_control
  echo '+cpu +memory +pids' >$cg/user.slice/tester/cgroup.subtree_control
  for d in user.slice/tester user.slice/tester/session; do
    chown tester $cg/$d $cg/$d/cgroup.procs $cg/$d/cgroup.subtree_control $cg/$d/cgroup.threads
  done
fi
uid=$(id -u tester)
install -d -m 700 -o tester -g tester /run/user/$uid
echo $$ >$cg/user.slice/tester/session/cgroup.procs
chown tester /out
as_tester() {
  setpriv --reuid tester --regid tester --init-groups env -i HOME=/home/tester USER=tester \
    PATH=/opt/shipyard/bin:/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin XDG_RUNTIME_DIR=/run/user/$uid \
    XDG_DATA_HOME=/tmp/tester-data XDG_CONFIG_HOME=/tmp/tester-config DISPLAY=:99 "$@"
}
# A busybox image (its one binary, static) and two containers, a compose
# project's: web depends on db.
rootfs=$(mktemp -d)
mkdir -p $rootfs/bin
cp /bin/busybox $rootfs/bin/busybox
for tool in sh sleep; do ln -s busybox $rootfs/bin/$tool; done
tar -C $rootfs -cf /tmp/rootfs.tar .
chmod 644 /tmp/rootfs.tar
as_tester shipyard import /tmp/rootfs.tar busy:1
compose=(--label com.docker.compose.project=shop --label com.docker.compose.config-hash=x)
as_tester shipyard run -d --name db "${compose[@]}" --label com.docker.compose.service=db busy:1 sleep 3600
as_tester shipyard run -d --name web "${compose[@]}" --label com.docker.compose.service=web \
  --label com.docker.compose.depends_on=db:service_started:false -p 18080:80 busy:1 sleep 3600
as_tester shipyard volume create kept >/dev/null
# The control: nothing serves tester's API before the app opens.
if [ -S /run/user/$uid/shipyard/api.sock ]; then echo "something serves tester's API already" >&2; exit 1; fi

echo "== the app, driven (a virtual display, Tauri's WebDriver)"
Xvfb :99 -screen 0 1440x900x24 -nolisten tcp &
for _ in $(seq 50); do [ -e /tmp/.X11-unix/X99 ] && break; sleep 0.1; done
[ -e /tmp/.X11-unix/X99 ] || { echo "Xvfb never started" >&2; exit 1; }
# The app is the driver's child: it gets SHIPYARD_BIN (the `shipyard` it
# runs) from the driver's environment.
as_tester SHIPYARD_BIN=/opt/shipyard/bin/shipyard dbus-run-session -- bash -c '
  tauri-driver --port 4444 >/out/tauri-driver.log 2>&1 &
  driver=$!
  for _ in $(seq 100); do (exec 3<>/dev/tcp/127.0.0.1/4444) 2>/dev/null && break; sleep 0.1; done
  (exec 3<>/dev/tcp/127.0.0.1/4444) 2>/dev/null || { echo "tauri-driver never listened: see tauri-driver.log" >&2; exit 1; }
  node /work/desktop/ui/tests-linux/drive.mjs /opt/shipyard/bin/shipyard-desktop /out
  status=$?
  kill $driver 2>/dev/null
  exit $status'
echo "== after: tester's service answers (the app started it)"
answer=$(as_tester curl -s --unix-socket /run/user/$uid/shipyard/api.sock http://localhost/_ping || true)
[ "$answer" = OK ] || { echo "tester's API doesn't answer after the app ran (said: ${answer:-nothing})" >&2; exit 1; }
# Its log is the one `--ensure` keeps beside the socket (systemd's socket
# unit would write none): the app started it, not something else.
[ -f /run/user/$uid/shipyard/service.log ] || { echo "no service.log: the app didn't start the service" >&2; exit 1; }
echo "it answers: the app started it"
