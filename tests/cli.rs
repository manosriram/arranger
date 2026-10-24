//! End to end through a real daemon: start a fake agent on its own worktree, answer its
//! permission ask, review and merge its work, then revive it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// A throwaway `$ARRANGER_HOME`; its daemon is stopped on drop.
struct Home(PathBuf);

impl Home {
    fn arranger(&self, dir: &Path, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_arranger")).current_dir(dir).env("ARRANGER_HOME", &self.0).env("ARRANGER_CLI", env!("CARGO_BIN_EXE_arranger")).args(args).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
    }

    /// `arranger ls`'s state column for task 1, once it is one of `want`.
    fn wait_state(&self, dir: &Path, want: &[&str]) -> String {
        let start = Instant::now();
        loop {
            let (_, ls) = self.arranger(dir, &["ls"]);
            let state = ls.lines().find(|l| l.starts_with("1 ")).and_then(|l| l.split_whitespace().nth(1)).unwrap_or("").to_string();
            if want.contains(&state.as_str()) {
                return state;
            }
            assert!(start.elapsed() < Duration::from_secs(15), "task 1 never got to {want:?}:\n{ls}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.arranger(&self.0, &["stop"]);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// The agent edits a file, asks to run a command and records the answer, then says it's done.
const AGENT: &str = r#"#!/bin/sh
echo "run $#" >> "$ARRANGER_HOME/runs"
echo hello > hello.txt
echo '{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"make test"}}' | "$ARRANGER_BIN" hook claude > "$ARRANGER_HOME/decision"
echo '{"hook_event_name":"Stop","last_assistant_message":"Added hello.txt"}' | "$ARRANGER_BIN" hook claude
"#;

#[test]
fn task_lifecycle() {
    let home = Home(std::env::temp_dir().join(format!("arranger-cli-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&home.0);
    let repo = home.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    // The daemon commits in a worktree without the helper's `git -c` flags.
    // Keep its identity in the repo so this also works on clean CI runners.
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@t"]);
    std::fs::write(repo.join("README"), "hi\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let agent = home.0.join("agent.sh");
    std::fs::write(&agent, AGENT).unwrap();
    std::fs::set_permissions(&agent, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let (ok, out) = home.arranger(&repo, &["new", agent.to_str().unwrap(), "say", "hello"]);
    assert!(ok, "{out}");
    assert_eq!(out.split_whitespace().next(), Some("1"));
    home.wait_state(&repo, &["needs-you"]);
    let (_, ls) = home.arranger(&repo, &["ls"]);
    assert!(ls.contains("arranger/1"), "{ls}");

    // Approving answers the hook, which prints claude's allow decision.
    assert!(home.arranger(&repo, &["approve", "1"]).0);
    home.wait_state(&repo, &["done", "exited"]);
    let decision = std::fs::read_to_string(home.0.join("decision")).unwrap();
    assert!(decision.contains(r#""behavior":"allow""#), "{decision}");
    let base_head = git(&repo, &["rev-parse", "HEAD"]);
    let (ok, out) = home.arranger(&repo, &["run", "1", "sh", "-c", "cat hello.txt; git branch --show-current"]);
    assert!(ok, "{out}");
    assert_eq!(out, "hello\narranger/1\n");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base_head);
    assert!(!repo.join("hello.txt").exists());
    assert!(!home.arranger(&repo, &["run", "999999", "true"]).0);
    let failed = Command::new(env!("CARGO_BIN_EXE_arranger"))
        .env("ARRANGER_HOME", &home.0).args(["run", "1", "sh", "-c", "exit 42"]).status().unwrap();
    assert_eq!(failed.code(), Some(42));
    let (_, events) = home.arranger(&repo, &["events", "1"]);
    assert!(events.contains("make test") && events.contains("Added hello.txt"), "{events}");

    let (ok, diff) = home.arranger(&repo, &["diff", "1"]);
    assert!(ok && diff.contains("arranger/1 -> main") && diff.contains("A hello.txt  +1 -0"), "{diff}");

    // Merge lands the work on main, which is checked out in the repo itself.
    let (ok, out) = home.arranger(&repo, &["commit", "1", "--merge", "add hello"]);
    assert!(ok, "{out}");
    assert_eq!(git(&repo, &["show", "main:hello.txt"]), "hello");
    assert_eq!(std::fs::read_to_string(repo.join("hello.txt")).unwrap(), "hello\n");
    let (_, diff) = home.arranger(&repo, &["diff", "1"]);
    assert!(!diff.contains("hello.txt"), "merged work is still up for review:\n{diff}");

    // An ended agent can be revived on its worktree, with a new prompt.
    home.wait_state(&repo, &["exited"]);
    assert!(home.arranger(&repo, &["revive", "1", "once", "more"]).0);
    home.wait_state(&repo, &["needs-you"]);
    let (ok, out) = home.arranger(&repo, &["revive", "1"]);
    assert!(!ok && out.contains("still running"), "{out}");
    assert_eq!(std::fs::read_to_string(home.0.join("runs")).unwrap().lines().count(), 2);
    assert!(home.arranger(&repo, &["deny", "1", "not now"]).0);
    home.wait_state(&repo, &["done", "exited"]);
    assert!(std::fs::read_to_string(home.0.join("decision")).unwrap().contains("not now"));
}

// A real non-TTY subprocess speaking app-server JSONL. No model calls or credentials.
const BACKGROUND_CODEX: &str = r#"#!/usr/bin/env python3
import json, os, sys
assert not sys.stdin.isatty() and not sys.stdout.isatty()
assert sys.argv[1:] == ['app-server', '--listen', 'stdio://']
def emit(value):
    print(json.dumps(value), flush=True)
def read():
    line = sys.stdin.readline()
    if not line: sys.exit(0)
    return json.loads(line)
def note(value):
    with open(os.environ['ARRANGER_HOME']+'/transport', 'a') as f: f.write(value+'\n')
assert read()['method'] == 'initialize'
emit({'id':0,'result':{}})
assert read()['method'] == 'initialized'
thread = read()
assert thread['method'] in ['thread/start','thread/resume']
if thread['method']=='thread/resume': assert thread['params']['threadId']=='thread-background'
note(thread['method'])
emit({'id':1,'result':{'thread':{'id':'thread-background'}}})
n = 0
while True:
    v = read()
    assert v['method']=='turn/start', v
    text = v['params']['input'][0]['text']
    n += 1
    turn = 'turn-'+str(n)
    emit({'id':v['id'],'result':{'turn':{'id':turn,'status':'inProgress'}}})
    emit({'method':'turn/started','params':{'turn':{'id':turn}}})
    if text == 'capacity':
        error = {'message':'Selected model is at capacity. Please try a different model.'}
        emit({'method':'error','params':{'error':error,'willRetry':False}})
        emit({'method':'item/completed','params':{'item':{'id':'cleanup','type':'commandExecution','status':'failed'}}})
        emit({'method':'turn/completed','params':{'turn':{'status':'failed','error':error}}})
        continue
    if text == 'retry':
        emit({'method':'error','params':{'error':{'message':'Selected model is at capacity. Please try a different model.'},'willRetry':True}})
        v = read()
        assert v['method']=='turn/interrupt'
        emit({'id':v['id'],'result':{}})
        emit({'method':'turn/completed','params':{'turn':{'status':'interrupted'}}})
        continue
    if text == 'cancel':
        v = read()
        assert v['method']=='turn/interrupt' and v['params']['turnId']==turn
        note('cancelled')
        emit({'id':v['id'],'result':{}})
        emit({'method':'turn/completed','params':{'turn':{'status':'interrupted'}}})
        continue
    if text == 'question':
        emit({'id':'questions','method':'item/tool/requestUserInput','params':{'questions':[{'id':'color','question':'Color?','options':[{'label':'Blue'}]},{'id':'size','question':'Size?','options':[]}]}})
        v = read()
        assert v == {'id':'questions','result':{'answers':{'color':{'answers':['Blue']},'size':{'answers':['Large']}}}}, v
        note('answered questions')
    elif text == 'hello':
        emit({'method':'item/started','params':{'item':{'id':'tool','type':'commandExecution','command':'cargo test'}}})
        emit({'method':'item/agentMessage/delta','params':{'itemId':'commentary','delta':'Checking the code…'}})
        emit({'id':'approve','method':'item/commandExecution/requestApproval','params':{'command':'cargo test'}})
        v = read()
        assert v == {'id':'approve','result':{'decision':'accept'}}, v
        emit({'method':'item/completed','params':{'item':{'id':'tool','type':'commandExecution','status':'completed','aggregatedOutput':'tests passed'}}})
    emit({'method':'item/agentMessage/delta','params':{'itemId':'reply-'+str(n),'delta':'Hello '}})
    emit({'method':'item/agentMessage/delta','params':{'itemId':'reply-'+str(n),'delta':'from the background'}})
    emit({'method':'item/completed','params':{'item':{'id':'reply-'+str(n),'type':'agentMessage','text':'Hello from the background'}}})
    emit({'method':'turn/completed','params':{'turn':{'status':'completed'}}})
    if text=='exit': sys.exit(0)
"#;

fn rpc(home: &Home, request: arranger::proto::Request) -> arranger::proto::Reply {
    use std::io::Write;
    let mut stream = std::os::unix::net::UnixStream::connect(home.0.join("arranger.sock")).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.write_all(&arranger::proto::frame(&request)).unwrap();
    arranger::proto::read_msg(&mut stream).unwrap()
}

#[test]
fn background_conversation_lifecycle() {
    use arranger::proto::{self, Event, Reply, Request};
    use std::io::Write;
    let home = Home(std::env::temp_dir().join(format!("arranger-background-{}", std::process::id())));
    let repo = home.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "hi\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let agent = home.0.join("codex");
    std::fs::write(&agent, BACKGROUND_CODEX).unwrap();
    std::fs::set_permissions(&agent, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let (ok, out) = home.arranger(&repo, &["new", agent.to_str().unwrap(), "hello"]);
    assert!(ok, "{out}");
    home.wait_state(&repo, &["needs-you"]);

    // Subscription delivers saved messages before any new ones, with no terminal snapshot.
    let mut chat = std::os::unix::net::UnixStream::connect(home.0.join("arranger.sock")).unwrap();
    chat.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    chat.write_all(&proto::frame(&Request::Conversation { id: 1 })).unwrap();
    let Reply::Events(events) = proto::read_msg(&mut chat).unwrap() else { panic!("missing history") };
    assert!(events.iter().any(|(_,e)| matches!(e,Event::Message {text,..} if text=="Checking the code…")));
    assert!(matches!(rpc(&home, Request::Answer { id: 1, allow: true, message: String::new() }), Reply::Ok));
    loop {
        let Reply::ChatEvent(_, event) = proto::read_msg(&mut chat).unwrap() else { panic!("unexpected terminal reply") };
        if event == Event::TurnDone { break; }
    }
    home.wait_state(&repo, &["done"]);
    let Reply::Events(events) = rpc(&home, Request::Events { id: 1 }) else { panic!("missing events") };
    assert!(events.iter().any(|(_,e)| matches!(e,Event::ToolOutput {text} if text=="tests passed")));
    assert!(events.iter().any(|(_,e)| matches!(e,Event::Message {text,append:false,..} if text=="Hello from the background")));

    assert!(matches!(rpc(&home,Request::Prompt {id:1,text:"question".into()}),Reply::Ok));
    home.wait_state(&repo, &["needs-you"]);
    assert!(matches!(rpc(&home,Request::Prompt {id:1,text:"Blue".into()}),Reply::Ok));
    let Reply::Tasks(tasks) = rpc(&home,Request::List {files:false}) else { panic!() };
    assert!(matches!(&tasks[0].need,Some(Event::Question {text,..}) if text=="Size?"));
    assert!(matches!(rpc(&home,Request::Prompt {id:1,text:"Large".into()}),Reply::Ok));
    home.wait_state(&repo, &["done"]);

    // Capacity failures stay out of working even after tool cleanup and turn completion.
    assert!(matches!(rpc(&home,Request::Prompt {id:1,text:"capacity".into()}),Reply::Ok));
    home.wait_state(&repo, &["needs-you"]);
    let Reply::Tasks(tasks) = rpc(&home,Request::List {files:false}) else { panic!() };
    assert!(matches!(&tasks[0].need,Some(Event::Error {message}) if message.contains("Selected model is at capacity")));
    assert!(matches!(rpc(&home,Request::Prompt {id:1,text:"retry".into()}),Reply::Ok));
    home.wait_state(&repo, &["retrying"]);
    assert!(matches!(rpc(&home,Request::Cancel {id:1}),Reply::Ok));
    assert!(matches!(rpc(&home,Request::Prompt {id:1,text:"cancel".into()}),Reply::Ok));
    assert!(matches!(rpc(&home,Request::Cancel {id:1}),Reply::Ok));
    // Wait for the process's cancellation acknowledgement before starting the next turn.
    let start=Instant::now();
    loop {
        if std::fs::read_to_string(home.0.join("transport")).unwrap().contains("cancelled") { break; }
        assert!(start.elapsed()<Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(matches!(rpc(&home,Request::Prompt {id:1,text:"exit".into()}),Reply::Ok));
    home.wait_state(&repo, &["exited"]);
    assert!(matches!(rpc(&home,Request::Prompt {id:1,text:"again".into()}),Reply::Ok));
    home.wait_state(&repo, &["done"]);
    assert_eq!(std::fs::read_to_string(home.0.join("transport")).unwrap(),"thread/start\nanswered questions\ncancelled\nthread/resume\n");
    // Reopening the conversation includes both runs and all prompts.
    let Reply::Events(events) = rpc(&home,Request::Conversation {id:1}) else { panic!() };
    assert_eq!(events.iter().filter(|(_,e)| matches!(e,Event::Prompt {..})).count(),9);
    assert!(matches!(rpc(&home,Request::Attach {id:1,cols:80,rows:24,native:true}),Reply::Error(_)));
}
