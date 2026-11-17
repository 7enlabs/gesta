import subprocess, re, sys

ZF = "/tmp/zfm4"

def sh(c):
    return subprocess.run(c, shell=True, capture_output=True, text=True)

def commit(a):
    o = sh(f"{ZF} changeset create m --author {a}")
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", o.stdout)
    if not m:
        return False
    sh(f"{ZF} changeset commit {m.group(1)}")
    return True

def w(t):
    open("f.txt", "w").write(t)

def own():
    out = sh(f"{ZF} blame f.txt").stdout.strip().splitlines()
    return [l.split()[1] for l in out]

cases = []

w("alpha\nbeta\ngamma\n"); commit("alice")
w("alpha\nBETA\ngamma\n"); commit("bob")
w("alpha\nBETA\ngamma\ndelta\n"); commit("carol")
cases.append(("basic replace+append", own(), ["alice", "bob", "alice", "carol"]))

sh("rm -f f.txt")
w("same\nx\nsame\n"); commit("alice")
w("same\nx\nsame\ny\n"); commit("bob")
cases.append(("duplicate lines", own(), ["alice", "alice", "alice", "bob"]))

sh("rm -f f.txt")
w("one\ntwo\nthree\n"); commit("alice")
w("one\nINSERTED\ntwo\nthree\n"); commit("bob")
cases.append(("insert middle", own(), ["alice", "bob", "alice", "alice"]))
w("one\ntwo\nthree\n"); commit("carol")
cases.append(("delete middle", own(), ["alice", "alice", "alice"]))

sh("rm -f f.txt")
w("A\nB\nC\n"); commit("dave")
w("B\nC\nA\n"); commit("erin")
cases.append(("reorder", own(), ["dave", "dave", "dave"]))

sh("rm -f f.txt")
w("l1\nl2\nl3\nl4\nl5\n"); commit("frank")
w("l1\nNEW\nl2\nl3\nl4\nl5\n"); commit("grace")
w("l1\nNEW\nl3\nl4\nl5\n"); commit("heidi")
cases.append(("insert then delete", own(), ["frank", "grace", "frank", "frank", "frank"]))

ok = True
for name, got, want in cases:
    good = got == want
    ok = ok and good
    print(f"{name:22} {'PASS' if good else 'FAIL'}  got={got}")

print()
print("ALL BLAME CASES:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
