"""Drives `synt` on a pseudo-terminal for apps/cli/tests/tui_pty.rs (python3 standard library only).

usage: tui_pty_driver.py scenario.json out.txt
scenario: {"argv": [...], "env": {...}, "steps": [["send", "bytes"] | ["wait", secs] |
           ["expect", "text", secs] | ["expect_new", "text", secs] | ["quiet", secs]]}
`expect_new` looks only at output that arrived after the last `send`. out.txt gets the whole
screen output (escape sequences removed) and the exit state, the last line is `STATE ok|failed <exit>`.
"""
import pty,os,sys,time,select,re,json,struct,fcntl,termios,signal
sc=json.load(open(sys.argv[1]))
env={"PATH":os.environ["PATH"],"TERM":"xterm-256color","COLORTERM":"truecolor"}
env.update(sc["env"])
pid,fd=pty.fork()
if pid==0:
    os.execve(sc["argv"][0],sc["argv"],env)
fcntl.ioctl(fd,termios.TIOCSWINSZ,struct.pack("HHHH",sc.get("rows",40),sc.get("cols",120),0,0))
buf=b""
def rd(t):
    global buf
    end=time.time()+t
    while time.time()<end:
        r,_,_=select.select([fd],[],[],0.1)
        if r:
            try:
                d=os.read(fd,65536)
            except OSError:
                return False
            if not d: return False
            buf+=d
    return True
def text():
    t=re.sub(rb"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07|\x1b[()][0-9A-B]",b" ",buf)
    return t.decode("utf8","replace")
def hang(*_):
    # a hard stop, so a stuck scenario fails in a minute and not at the CI step limit
    try: os.killpg(pid,9)
    except OSError: pass
    open(sys.argv[2],"w").write(text()+"\nSTATE failed driver deadline\n")
    os._exit(1)
signal.signal(signal.SIGALRM,hang); signal.alarm(sc.get("deadline",120))
ok=True
killed=False
sent_at=0
for st in sc["steps"]:
    if st[0]=="send": sent_at=len(buf); os.write(fd,st[1].encode().decode("unicode_escape").encode("latin1")); rd(0.2)
    elif st[0]=="wait": rd(st[1])
    elif st[0]=="kill":
        # SIGKILL, as when the terminal window is closed: no shutdown code runs
        # Do not block in waitpid: on macOS a dying session leader waits for its tty output to
        # drain, so keep reading the master until it is reaped (hard limit 10 s).
        for f in (lambda: os.killpg(pid,9), lambda: os.kill(pid,9)):
            try: f()
            except OSError: pass
        killed=True
        end=time.time()+10
        while time.time()<end:
            rd(0.1)
            try:
                if os.waitpid(pid,os.WNOHANG)[0]==pid: break
            except ChildProcessError: break
        else:
            print("kill: child not reaped in 10 s")
    elif st[0]=="quiet":
        # no output for st[1] seconds; capped, because a spinner never goes quiet
        end=time.time()+20
        while time.time()<end:
            n=len(buf); rd(st[1])
            if len(buf)==n: break
    elif st[0] in ("expect","expect_new"):
        end=time.time()+st[2]; mark=sent_at if st[0]=="expect_new" else 0
        found=False
        while time.time()<end:
            rd(0.2)
            t=re.sub(rb"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07",b" ",buf[mark:]).decode("utf8","replace")
            if st[1] in t: found=True;break
        if not found:
            ok=False; print("FAIL step",st); break
# exit
time.sleep(0.5)
if killed: p,stt=pid,"killed"
else:
    try:
        p,stt=os.waitpid(pid,os.WNOHANG)
    except Exception as e: p,stt=0,None
if p==0:
    os.write(fd,b"\x04"); rd(1.0)
    try: p,stt=os.waitpid(pid,os.WNOHANG)
    except Exception: pass
    if p==0:
        os.kill(pid,9); stt="killed"
open(sys.argv[2],"w").write(text()+"\nSTATE "+("ok" if ok else "failed")+" "+str(stt)+"\n")
