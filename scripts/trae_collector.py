#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Trae -> TokenMonitor 采集脚本（Windows）

流程（对应既定方案）：
  1. 检查 %APPDATA%\\TokenMonitor\\trae_key.json 中的缓存 key；
     —— 用「当前 Trae 主库 page1 的 salt + HMAC」校验，通过即复用（快路径 ~1s）。
  2. 缓存不存在 / HMAC 校验失败（Trae 重启换 key、库被更换）才扫 Trae 进程内存提 key；
     —— 提 key 失败明确提示「请先发起一次 agent 任务再重试」。
  3. 解密 Trae 主库到临时明文库。
     —— 不手工合并 WAL：Trae 活跃写入时 WAL 快照与 db 快照时间点不一致，
        手工套帧会让 integrity_check 失败；Trae 自身 checkpoint 会把最新数据
        并入主库，只读主库即可拿到稳定且最新的表（已实测）。
  4. 解析 chat_turn.context.token_usage，按 fingerprint 去重写入
     tokenmonitor.db 的 usage_records（provider='trae'）。
     —— fingerprint = localdb:{session_id}:{created_at}:{id}；
        表已有 UNIQUE 约束 + INSERT OR IGNORE，重复运行不会产生重复行。

用法：
  python trae_collector.py                 # 采集（缓存 key 优先）
  python trae_collector.py --check         # 只检查缓存 key 状态
  python trae_collector.py --force-extract # 强制重新从内存提 key 并刷新缓存
  python trae_collector.py --dry-run       # 预览解析结果，不写库
  python trae_collector.py --auto          # 自动采集（主程序每轮扫描时拉起）
      —— 只走缓存 key：命中即解密+幂等写库；失效立即静默退出（不扫内存、不提示）。
         退出码 0=成功/无新数据，2=缓存失效跳过，其它=真错误。

依赖：pycryptodome（python -m pip install pycryptodome）
"""

import os, sys, json, shutil, struct, hashlib, hmac as hm_mod
import sqlite3, tempfile, time, subprocess, re, ctypes, datetime
from ctypes import wintypes

try:
    from Crypto.Cipher import AES
except ImportError:
    print("缺少依赖 pycryptodome，请先执行：python -m pip install pycryptodome", flush=True)
    sys.exit(3)

# ------------------------------------------------------------------ 配置 ---
APPDATA = os.environ.get("APPDATA") or os.path.join(os.path.expanduser("~"), "AppData", "Roaming")
TRAE_DIR = os.path.join(APPDATA, "Trae CN", "ModularData", "ai-agent")
TRAE_DB = os.path.join(TRAE_DIR, "database.db")
TM_DB = os.path.join(APPDATA, "TokenMonitor", "tokenmonitor.db")
KEY_CACHE = os.path.join(APPDATA, "TokenMonitor", "trae_key.json")
WORK = os.path.join(tempfile.gettempdir(), "tm_trae_collector")

PAGE_SZ, SALT_SZ, RESERVE_SZ, IV_SZ, HMAC_SZ = 4096, 16, 80, 16, 64
CRYPT_SZ = PAGE_SZ - RESERVE_SZ            # 4016
SQLITE_HDR = b"SQLite format 3\x00"
KEY_SZ = 32
PROC_HINTS = ('"trae cn.exe"', 'teleagent.exe', '"ais.exe"')
MAX_SCAN_SECONDS = 60    # 单次提 key 的总时长上限，超时即走「请先发起一次 agent 任务」提示


def log(*a):
    print(*a, flush=True)


class ExtractError(Exception):
    """从进程内存提取 key 失败。"""


# --------------------------------------------------- 数据库头 / key 校验 ---
class DbHeader:
    """保存校验 key 所需的 page1 材料；verify() 即为「key 能否解开当前库」。"""

    def __init__(self, db_path=TRAE_DB):
        with open(db_path, "rb") as f:
            p1 = f.read(PAGE_SZ)
        self.salt = p1[:SALT_SZ]
        self.mac_salt = bytes(b ^ 0x3A for b in self.salt)
        self.hmac_data = p1[SALT_SZ:PAGE_SZ - RESERVE_SZ + IV_SZ]   # [16:4032]
        self.stored_hmac = p1[PAGE_SZ - HMAC_SZ:PAGE_SZ]            # [4032:4096]
        self.size = os.path.getsize(db_path)
        self._cache = {}

    def verify(self, key: bytes) -> bool:
        if not key or len(key) != KEY_SZ:
            return False
        if key in self._cache:
            return self._cache[key]
        mk = hashlib.pbkdf2_hmac("sha512", key, self.mac_salt, 2, dklen=KEY_SZ)
        ok = any(
            hm_mod.new(mk, data, hashlib.sha512).digest() == self.stored_hmac
            for data in (self.hmac_data,
                         self.hmac_data + struct.pack(">I", 1),
                         self.hmac_data + struct.pack("<I", 1))
        )
        self._cache[key] = ok
        return ok


# ------------------------------------------------------------ key 缓存层 ---
def load_cached_key(hdr: DbHeader):
    """返回 (key|None, 说明)。校验用的是「当前库」，不是缓存里的旧值。"""
    if not os.path.exists(KEY_CACHE):
        return None, "缓存文件不存在"
    try:
        with open(KEY_CACHE, "r", encoding="utf-8") as f:
            obj = json.load(f)
        key = bytes.fromhex(obj["key_hex"])
    except Exception as e:
        return None, "缓存文件损坏 (%s)" % e
    if not hdr.verify(key):
        return None, "缓存 key 对当前数据库 HMAC 校验失败（Trae 重启换 key 或库已更换）"
    return key, "缓存 key HMAC 校验通过"


def save_cached_key(key: bytes, hdr: DbHeader):
    os.makedirs(os.path.dirname(KEY_CACHE), exist_ok=True)
    obj = {
        "key_hex": key.hex(),
        "salt_hex": hdr.salt.hex(),
        "db_size": hdr.size,
        "verified_at": datetime.datetime.now().isoformat(timespec="seconds"),
    }
    tmp = KEY_CACHE + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(obj, f, ensure_ascii=False, indent=2)
    os.replace(tmp, KEY_CACHE)


# --------------------------------------------------------- 内存提 key 层 ---
def _target_pids():
    out = subprocess.run(["tasklist", "/FO", "CSV", "/NH"],
                         capture_output=True, timeout=30).stdout.decode("gbk", errors="replace")
    pids = []
    for line in out.splitlines():
        if any(k in line.lower() for k in PROC_HINTS):
            try:
                pids.append(int(line.split('","')[1]))
            except Exception:
                pass
    return pids


def _setup_win32():
    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    k32.OpenProcess.restype = wintypes.HANDLE
    k32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    k32.CloseHandle.argtypes = [wintypes.HANDLE]
    k32.ReadProcessMemory.restype = wintypes.BOOL
    k32.ReadProcessMemory.argtypes = [wintypes.HANDLE, wintypes.LPCVOID, wintypes.LPVOID,
                                      ctypes.c_size_t, ctypes.POINTER(ctypes.c_size_t)]
    k32.VirtualQueryEx.restype = ctypes.c_size_t
    k32.VirtualQueryEx.argtypes = [wintypes.HANDLE, wintypes.LPCVOID, wintypes.LPVOID, ctypes.c_size_t]

    class MBI(ctypes.Structure):
        _fields_ = [("BaseAddress", ctypes.c_void_p), ("AllocationBase", ctypes.c_void_p),
                    ("AllocationProtect", wintypes.DWORD), ("PartitionId", wintypes.WORD),
                    ("RegionSize", ctypes.c_size_t), ("State", wintypes.DWORD),
                    ("Protect", wintypes.DWORD), ("Type", wintypes.DWORD)]
    return k32, MBI


def _scan_regions(regions, hdr: DbHeader):
    """两阶段：只读内存，再只在 salt 锚点附近验证 key（避免对整块跑正则）。"""
    salt = hdr.salt
    salt_ascii = salt.hex().encode()
    x96_re = re.compile(rb"x'([0-9a-fA-F]{64,192})'")

    def hit(kc):
        return kc if hdr.verify(kc) else None

    for data in regions:
        # 1) 二进制 salt 锚点：key 通常在 salt 前 16/32/48/64/96/128 字节，或紧随其后
        pos = 0
        while True:
            i = data.find(salt, pos)
            if i < 0:
                break
            pos = i + 1
            for back in (32, 64, 96, 48, 16, 128):
                if i - back >= 0:
                    got = hit(data[i - back:i])
                    if got:
                        return got
            if i + SALT_SZ + 32 <= len(data):
                got = hit(data[i + SALT_SZ:i + SALT_SZ + 32])
                if got:
                    return got
        # 2) salt 的 ascii-hex 写法，key 在其前的 hex 串里
        pos = 0
        while True:
            i = data.find(salt_ascii, pos)
            if i < 0:
                break
            pos = i + 1
            for back in (64, 96, 128):
                if i - back >= 0:
                    s = data[i - back:i]
                    if len(s) == back and re.fullmatch(rb"[0-9a-fA-F]+", s):
                        for j in range(0, len(s) - 63, 32):
                            try:
                                got = hit(bytes.fromhex(s[j:j + 64].decode()))
                            except Exception:
                                continue
                            if got:
                                return got
        # 3) x'<64..192 hex>' 模式（参考实现）
        for m in x96_re.finditer(data):
            s = m.group(1).decode(errors="ignore")
            for j in range(0, max(1, len(s) - 63), 32):
                kc = s[j:j + 64]
                if len(kc) == 64:
                    try:
                        got = hit(bytes.fromhex(kc))
                    except Exception:
                        continue
                    if got:
                        return got
    return None


def extract_key_from_memory(hdr: DbHeader):
    pids = _target_pids()
    if not pids:
        log("[key] 未发现 Trae / TeleAgent 进程")
        raise ExtractError()
    log("[key] 目标进程 %d 个，开始扫描内存..." % len(pids))
    k32, MBI = _setup_win32()
    READABLE = {0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80}
    readable_regions = 0
    salt = hdr.salt
    t0 = time.perf_counter()
    for pid in pids:
        if time.perf_counter() - t0 > MAX_SCAN_SECONDS:
            log("[key] 扫描超过 %ds 上限" % MAX_SCAN_SECONDS)
            break
        h = k32.OpenProcess(0x0010 | 0x0400, False, pid)   # VM_READ | QUERY_INFO
        if not h:
            continue
        regions = []
        addr = ctypes.c_void_p(0x10000)
        mbi = MBI()
        while k32.VirtualQueryEx(h, addr, ctypes.byref(mbi), ctypes.sizeof(mbi)):
            base, size = mbi.BaseAddress or 0, mbi.RegionSize
            if mbi.State == 0x1000 and mbi.Protect in READABLE and 0 < size < 600 * 1024 * 1024:
                buf = (ctypes.c_ubyte * size)()
                got = ctypes.c_size_t()
                if k32.ReadProcessMemory(h, ctypes.c_void_p(base), buf, size, ctypes.byref(got)) and got.value:
                    chunk = bytes(buf[:got.value])
                    readable_regions += 1
                    # 预筛：只保留含 salt 二进制锚点的块，避免对整块跑正则（性能关键）
                    if salt in chunk:
                        regions.append(chunk)
            addr = ctypes.c_void_p(base + size)
            if base + size > 0x7FFFFFFFFFFF:
                break
        k32.CloseHandle(h)
        found = _scan_regions(regions, hdr)
        if found:
            log("[key] 命中（PID %d，耗时 %.1fs）" % (pid, time.perf_counter() - t0))
            return found
    if readable_regions == 0:
        log("[key] 进程存在但没有可读内存块 —— 可能是权限不足（请用管理员权限运行）")
    raise ExtractError()


def get_key(hdr: DbHeader, force: bool = False):
    if not force:
        key, why = load_cached_key(hdr)
        if key:
            log("[key] " + why + " -> 复用缓存，跳过内存扫描")
            return key
        log("[key] " + why)
    else:
        log("[key] --force-extract：忽略缓存，强制重新提取")
    log("[key] 从 Trae 进程内存提取 key ...")
    try:
        key = extract_key_from_memory(hdr)
    except ExtractError:
        log("")
        log("✗ 提 key 失败：请先发起一次 agent 任务再重试")
        log("  说明：SQLCipher key 只在 agent 任务运行时驻留 Trae 进程内存；")
        log("        若 Trae 刚启动或近期无 agent 任务，内存里没有 key 可提。")
        return None
    log("[key] 提取成功，写入缓存：%s" % KEY_CACHE)
    save_cached_key(key, hdr)
    return key


# ------------------------------------------------------------- 解密主库 ---
def decrypt_main_db(key: bytes):
    os.makedirs(WORK, exist_ok=True)
    work_db = os.path.join(WORK, "database.db")
    # 快照稳定性检测：活跃写入下偶发 db 正在写，重试到 size+mtime 稳定
    for attempt in range(6):
        s1 = os.stat(TRAE_DB)
        shutil.copy2(TRAE_DB, work_db)
        s2 = os.stat(TRAE_DB)
        if (s1.st_size, s1.st_mtime_ns) == (s2.st_size, s2.st_mtime_ns):
            break
        time.sleep(0.25)
    with open(work_db, "rb") as f:
        data = f.read()
    dec = bytearray()
    for i in range(len(data) // PAGE_SZ):
        raw = data[i * PAGE_SZ:(i + 1) * PAGE_SZ]
        if len(raw) < PAGE_SZ:
            break
        iv = raw[CRYPT_SZ:CRYPT_SZ + IV_SZ]
        if i == 0:
            pt = AES.new(key, AES.MODE_CBC, iv).decrypt(raw[SALT_SZ:CRYPT_SZ])
            dec += SQLITE_HDR + pt + b"\x00" * RESERVE_SZ
        else:
            pt = AES.new(key, AES.MODE_CBC, iv).decrypt(raw[:CRYPT_SZ])
            dec += pt + b"\x00" * RESERVE_SZ
    plain = os.path.join(WORK, "main_plain.db")
    with open(plain, "wb") as f:
        f.write(dec)
    return plain


# ------------------------------------------------------- 解析 / 入库层 ---
def walk_tokens(obj):
    out = {}
    if isinstance(obj, dict):
        if isinstance(obj.get("token_usage"), dict):
            out.update(obj["token_usage"])
        for v in obj.values():
            for k, vv in walk_tokens(v).items():
                out.setdefault(k, vv)
    elif isinstance(obj, list):
        for v in obj:
            for k, vv in walk_tokens(v).items():
                out.setdefault(k, vv)
    return out


def get_model(ctx):
    def rec(o):
        if isinstance(o, dict):
            for k in ("config_name", "model_name", "model", "modelId", "model_id"):
                if isinstance(o.get(k), str):
                    return o[k]
            for v in o.values():
                x = rec(v)
                if x:
                    return x
        elif isinstance(o, list):
            for v in o:
                x = rec(v)
                if x:
                    return x
        return None
    return rec(ctx)


def parse_turns(plain_db):
    con = sqlite3.connect("file:" + plain_db + "?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    turns = []
    for r in con.execute("SELECT id,session_id,turn_id,created_at,context FROM chat_turn "
                         "WHERE context IS NOT NULL AND context!=''"):
        raw = r["context"]
        try:
            ctx = json.loads(raw) if isinstance(raw, str) else raw
        except Exception:
            try:
                ctx = json.loads(str(raw))
            except Exception:
                continue
        tu = walk_tokens(ctx)
        if not tu:
            continue

        def g(*names):
            for n in names:
                if isinstance(tu.get(n), (int, float)):
                    return int(tu[n])
            return 0

        inp = g("prompt_tokens", "input_tokens")
        out = g("completion_tokens", "output_tokens")
        cr = g("cache_read_input_tokens", "cache_read_tokens")
        cw = g("cache_creation_input_tokens", "cache_write_tokens")
        total = g("total_tokens") or (inp + out)
        if total == 0 and inp + out + cr + cw == 0:
            continue
        model = (get_model(ctx) or "trae-unknown").strip().lower()
        sid = (r["session_id"] or "") or (r["turn_id"] or "") or str(r["id"])
        turns.append({"id": r["id"], "session_id": sid, "created_at": int(r["created_at"]),
                      "model": model, "input": inp, "output": out, "cache_read": cr, "cache_write": cw})
    con.close()
    return turns


def build_rows(turns):
    rows, seen = [], set()
    for t in turns:
        started = datetime.datetime.fromtimestamp(t["created_at"], datetime.timezone.utc).isoformat()
        fp = "localdb:%s:%d:%s" % (t["session_id"], t["created_at"], t["id"])
        if fp in seen:
            continue
        seen.add(fp)
        rows.append(("trae", "Trae", t["session_id"], t["model"], started,
                     t["input"], t["output"], t["cache_read"], t["cache_write"], 0, 0, fp))
    return rows


def import_rows(rows, commit=True):
    if not os.path.exists(TM_DB):
        log("✗ TokenMonitor 数据库不存在：%s" % TM_DB)
        return None
    con = sqlite3.connect(TM_DB, timeout=15)
    con.execute("PRAGMA busy_timeout=15000")
    before = con.execute("SELECT count(*) FROM usage_records WHERE provider='trae'").fetchone()[0]
    if not commit:
        con.close()
        return {"before": before, "inserted": 0, "after": before}
    ins = 0
    for row in rows:
        cur = con.execute("""INSERT OR IGNORE INTO usage_records
            (provider,project,session_id,model,started_at,input_tokens,output_tokens,
             cache_read_tokens,cache_write_tokens,cost_micros,raw_bytes,fingerprint)
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?)""", row)
        ins += cur.rowcount
    con.commit()
    after = con.execute("SELECT count(*) FROM usage_records WHERE provider='trae'").fetchone()[0]
    con.close()
    return {"before": before, "inserted": ins, "after": after}


# ------------------------------------------------------------------ main ---
def main():
    args = sys.argv[1:]
    force = "--force-extract" in args
    dry = "--dry-run" in args
    check_only = "--check" in args
    auto = "--auto" in args     # 主程序每轮扫描拉起：只用缓存 key，失效静默跳过

    if not os.path.exists(TRAE_DB):
        if auto:
            return 0            # 没装 Trae：静默无事，不打扰
        log("✗ 未找到 Trae 数据库：%s" % TRAE_DB)
        return 1

    if auto:
        # --auto 是后台自动模式：全程静默，只在真出错时写一行供排查。
        # 关键约束：绝不扫内存提 key（那要管理员权限 + ≤60s + 近期有 agent 任务，
        # 会拖垮 5 分钟一轮的扫描）。缓存 key 用当前库 salt+HMAC 校验，失效即退。
        try:
            hdr = DbHeader()
            key, why = load_cached_key(hdr)
            if not key:
                return 2        # 缓存失效/不存在 → 静默跳过，等用户手动刷新缓存
            plain = decrypt_main_db(key)
            probe = sqlite3.connect("file:" + plain + "?mode=ro", uri=True)
            ic = probe.execute("PRAGMA integrity_check").fetchone()[0]
            probe.close()
            if ic != "ok":
                return 0        # 主库正在写入的瞬态，跳过本轮，下次再来
            turns = parse_turns(plain)
            rows = build_rows(turns)
            if not dry and rows:
                import_rows(rows, commit=True)
            return 0
        except Exception as e:
            # 自动模式不向 UI 抛错（子进程 stdout 被丢弃），仅留一行到 stderr 便于排查
            print("[auto] Trae 采集跳过: %s" % e, file=sys.stderr, flush=True)
            return 6

    hdr = DbHeader()
    log("Trae DB : %s (%d bytes)" % (TRAE_DB, hdr.size))

    if check_only:
        key, why = load_cached_key(hdr)
        log("[check] 缓存路径: %s" % KEY_CACHE)
        log("[check] " + why + ("（可复用）" if key else "（需重新提取）"))
        return 0 if key else 2

    key = get_key(hdr, force=force)
    if not key:
        return 2

    log("[dec] 解密 Trae 主库 ...")
    plain = decrypt_main_db(key)
    probe = sqlite3.connect("file:" + plain + "?mode=ro", uri=True)
    ic = probe.execute("PRAGMA integrity_check").fetchone()[0]
    probe.close()
    if ic != "ok":
        log("✗ 明文库 integrity_check 失败：%s（主库快照可能正在写入，请稍后重试）" % ic)
        return 4

    turns = parse_turns(plain)
    rows = build_rows(turns)
    log("[parse] 有效 turn=%d，去重后行=%d" % (len(turns), len(rows)))
    tot = sum(r[5] + r[6] + r[7] + r[8] for r in rows)
    log("[parse] 合计 token=%d" % tot)

    if dry:
        log("[dry-run] 不写库。")
        return 0

    res = import_rows(rows, commit=True)
    if res is None:
        return 5
    log("[db] 写入完成：inserted=%d，trae 行 %d -> %d" %
        (res["inserted"], res["before"], res["after"]))
    log("完成。若 TokenMonitor 正在运行，请刷新/重启以重载报表。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
