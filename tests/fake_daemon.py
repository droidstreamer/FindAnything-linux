# Canned daemon replies for GUI snapshots of states this laptop can't reach (7 drives, indexing):
#   python3 -I tests/fake_daemon.py SOCKET busy|idle   then FA_SOCKET=SOCKET FA_GUI_SNAPSHOT=... fa-gui
import os, socket, sys
sock, mode = sys.argv[1], sys.argv[2]
D = [("/", 412880), ("/home", 1204551), ("/mnt/drives/BACKUP", 2961040), ("/mnt/drives/DATA", 3880112),
     ("/mnt/drives/PHOTOS", 1502377), ("/mnt/drives/GAMES", 5850054), ("/media/user/STICK", 18442)]
busy = mode == "busy"
cur = 2  # 0-based: BACKUP
status = ("Find Anything 0.21.0, up 680s, memory 1782345 kB\n14887739 entries in 1203968 directories, generation 9\n"
          "log /var/log/findanything.log\nindex file /var/cache/findanything/index.bin (812 MB, saved 3 min ago)\n"
          + ("startup: started over\n" if busy else "startup: started over, 7 of 7 drives read from scratch, ready in 36.4 s\n")
          + "detailed log on\n"
          + (f"busy: checking {D[cur][0]} ({cur+1} of 7 drives), 1,128,195 of about 2,961,040 files read, 12 s on this drive\n" if busy else "")
          + "change queue: up to 1,717,986 changes waiting (2% of memory)\nchanges waiting: 0\n"
          "events 0 (renames 0), size/date updates 0, parent gone 0, parent not indexed 0, overflows 0, scan errors 0\n")
drives = "".join(f"{p}\text4\t/dev/x\t{'checking' if busy and i >= cur else 'live'}\t{n if not busy or i < cur else (1128195 if i == cur else 0)}\tlive updates\t\n" for i, (p, n) in enumerate(D))
try: os.unlink(sock)
except FileNotFoundError: pass
s = socket.socket(socket.AF_UNIX); s.bind(sock); s.listen(8)
while True:
    c, _ = s.accept()
    req = b""
    while not req.endswith(b"\n"):
        d = c.recv(4096)
        if not d: break
        req += d
    r = req.decode().strip()
    out = status if r == "status" else drives if r == "drives" else "/mnt/drives/BACKUP/Temp\n" if r == "exclude" else ""
    c.sendall(out.encode()); c.close()
