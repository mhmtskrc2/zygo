import os, resource, subprocess, time
N = 400
uid = os.getuid()
S = f"/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/zygo.slice"
T = f"{S}/tenants/default"
def busy():
    v = list(map(int, open("/proc/stat").readline().split()[1:]))
    return (sum(v) - v[3] - v[4]) / os.sysconf("SC_CLK_TCK")
def m(label, fn, n=N):
    for _ in range(5): fn()
    r0 = resource.getrusage(resource.RUSAGE_SELF); c0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    b0, t0 = busy(), time.perf_counter()
    for _ in range(n): fn()
    t1, b1 = time.perf_counter(), busy()
    r1 = resource.getrusage(resource.RUSAGE_SELF); c1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    time.sleep(0.5); b2 = busy()
    own = (r1.ru_utime + r1.ru_stime - r0.ru_utime - r0.ru_stime + c1.ru_utime + c1.ru_stime - c0.ru_utime - c0.ru_stime)
    print(f"{label:<46} {own*1e3/n:6.3f} ms own CPU  {(b1-b0)*1e3/n:6.3f} ms VM CPU  {(t1-t0)*1e3/n:6.3f} ms wall  (+{(b2-b1)*1e3/n:5.3f} ms VM after)")
def probe():
    os.mkdir(f"{S}/p"); os.rmdir(f"{S}/p")
def nested():
    os.makedirs(f"{T}/f/g/z"); os.rmdir(f"{T}/f/g/z"); os.rmdir(f"{T}/f/g"); os.rmdir(f"{T}/f")
def leaf():
    os.mkdir(f"{T}/f"); os.rmdir(f"{T}/f")
def writes32():
    for _ in range(8):
        for c in ("cpu", "memory", "pids", "io"):
            with open(f"{T}/cgroup.subtree_control", "w") as f: f.write("+" + c)
def read1():
    open(f"{T}/cgroup.subtree_control").read()
home = os.path.expanduser("~/.local/share/zygo/tmp/x")
def ext4():
    os.mkdir(home); os.rmdir(home)
def tmpfs():
    os.mkdir(f"/run/user/{uid}/x"); os.rmdir(f"/run/user/{uid}/x")
run = lambda *a: (lambda: subprocess.run(a, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
m("cgroup: one probe mkdir+rmdir", probe)
m("cgroup: three nested mkdir+rmdir", nested)
m("cgroup: one leaf mkdir+rmdir", leaf)
m("cgroup: 32 no-op subtree_control writes", writes32)
m("cgroup: read subtree_control once", read1)
m("staging dir mkdir+rmdir, ext4", ext4)
m("staging dir mkdir+rmdir, tmpfs", tmpfs)
n = 150
m("process: /bin/true", run("/bin/true"), n)
m("process: zygo --version", run("/opt/zygo-bench/zygo", "--version"), n)
m("process: kern --version", run("/opt/zygo-bench/kern", "--version"), n)
m("userns: one uid mapped directly", run("unshare", "--user", "--map-root-user", "/bin/true"), n)
m("userns: uid+gid range via newuidmap/newgidmap", run("unshare", "--user", "--map-auto", "--map-root-user", "/bin/true"), n)
