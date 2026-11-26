"""No live credentials, login, browser, or provider calls."""
import fcntl
import importlib.util
import json
import os
import pathlib
import pty
import stat
import subprocess
import sys
import tempfile
import termios
import unittest
from unittest.mock import patch
ROOT=pathlib.Path(__file__).resolve().parents[1]
def load(name):
 s=importlib.util.spec_from_file_location(name, ROOT/'adapters'/f'{name}.py');m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
a=load('agent_meter_adapter');c=load('agent_meter_connect')
FAKE_AGY_KEYRING_SCRIPT="""#!/usr/bin/env python3
import json, os, pathlib, sys, time
home = pathlib.Path(os.environ["HOME"])
marker = home / ".gemini/antigravity-cli/cache/antigravity-keyring-unavailable"
calls_file = pathlib.Path(os.environ["FAKE_AGY_CALLS"])
oauth_file = pathlib.Path(os.environ["FAKE_AGY_OAUTH"])
calls = json.loads(calls_file.read_text()) if calls_file.exists() else []
calls.append({
    "argv": sys.argv[1:],
    "session_leader": os.getpid() == os.getsid(0),
    "dbus": os.environ.get("DBUS_SESSION_BUS_ADDRESS"),
    "xdg_runtime_dir": os.environ.get("XDG_RUNTIME_DIR"),
})
calls_file.write_text(json.dumps(calls))
use_file = False
if marker.exists():
    if time.time() - marker.stat().st_mtime < 3600:
        use_file = True
    else:
        marker.unlink()
if not use_file:
    state = os.environ.get("FAKE_KEYRING_STATE", "timeout")
    if state == "timeout":
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text("2026-11-25T00:00:00Z\\n")
        use_file = True
    elif state == "unlocked":
        print(json.dumps({
            "command": {
                "name": "usage",
                "data": {
                    "groups": [{
                        "name": "Gemini Models",
                        "buckets": [{
                            "name": "Five Hour Limit Remaining",
                            "window": "5h",
                            "remaining_fraction": 0.75,
                            "reset_time": "2026-11-26T00:00:00Z",
                        }],
                    }],
                },
            },
        }))
        raise SystemExit(0)
# Fallback file storage has no token; mimic printmode.waitForAuth(/dev/tty).
try:
    fd = os.open("/dev/tty", os.O_RDONLY)
    os.close(fd)
except OSError:
    sys.stderr.write("Error: authentication required. Run 'agy' to log in, then retry.\\n")
    raise SystemExit(1)
oauth_file.write_text("interactive OAuth launched\\n")
sys.stderr.write("Authentication required. Please visit the URL to log in:\\nWaiting for authentication...\\n")
raise SystemExit(1)
"""
class AuthLoop(unittest.TestCase):
 def setUp(self):
  self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
  self.env=patch.dict(os.environ,{'HOME':self.tmp.name,'XDG_STATE_HOME':self.tmp.name+'/state'});self.env.start();self.addCleanup(self.env.stop)
 def test_provider_marker_prevents_launch_without_reading_credentials(self):
  marker=pathlib.Path(self.tmp.name)/'.gemini/antigravity-cli/cache/antigravity-keyring-unavailable';marker.parent.mkdir(parents=True);marker.touch()
  with patch.object(a,'_cli_installed',return_value=True),patch.object(a.subprocess,'run') as run:
   self.assertEqual(a.agy()['reason'],'keyring_cooldown');run.assert_not_called()
  self.assertTrue(marker.exists())
 def test_all_provider_keyring_marker_paths_pause_polls(self):
  for rel in a._AGY_KEYRING_UNAVAILABLE_MARKERS:
   with self.subTest(marker=rel):
    for existing in a._agy_keyring_unavailable_markers():
     pathlib.Path(existing).unlink(missing_ok=True)
    marker=pathlib.Path(os.path.expanduser(rel));marker.parent.mkdir(parents=True,exist_ok=True);marker.touch()
    with patch.object(a,'_cli_installed',return_value=True),patch.object(a.subprocess,'run') as run:
     self.assertEqual(a.agy()['reason'],'keyring_cooldown');run.assert_not_called()
 def test_timeout_latches_subsequent_polls(self):
  with patch.object(a,'_cli_installed',return_value=True),patch.object(a.subprocess,'run',side_effect=subprocess.TimeoutExpired('agy',20)) as run:
   self.assertEqual(a.agy()['reason'],'timeout');self.assertEqual(a.agy()['reason'],'refresh_paused');self.assertEqual(run.call_count,1)
   self.assertEqual(run.call_args.kwargs['stdin'],subprocess.DEVNULL);self.assertTrue(run.call_args.kwargs['start_new_session']);self.assertLess(run.call_args.kwargs['timeout'],30)
  self.assertEqual(pathlib.Path(a._agy_reconnect_marker()).read_text(),'reconnect required\n')
 def test_expired_marker_allows_retry(self):
  marker=pathlib.Path(self.tmp.name)/'.gemini/antigravity-cli/cache/antigravity-keyring-unavailable';marker.parent.mkdir(parents=True);marker.touch();os.utime(marker,(0,0))
  self.assertIsNone(a._agy_pause_reason());self.assertTrue(marker.exists())
 def test_login_required_suspends_later_polls(self):
  err=subprocess.CalledProcessError(1,'agy',stderr='keyring timeout; interactive OAuth login required')
  with patch.object(a,'_cli_installed',return_value=True),patch.object(a.subprocess,'run',side_effect=err) as run:
   self.assertEqual(a.agy()['reason'],'login_required')
   self.assertEqual(a.agy()['reason'],'refresh_paused')
   self.assertEqual(a.agy()['reason'],'refresh_paused')
   self.assertEqual(run.call_count,1)
   self.assertEqual(run.call_args.kwargs['stdin'],subprocess.DEVNULL)
   self.assertTrue(run.call_args.kwargs['start_new_session'])
  self.assertTrue(pathlib.Path(a._agy_reconnect_marker()).exists())
 def test_plain_command_failure_stays_retryable(self):
  err=subprocess.CalledProcessError(2,'agy',stderr='language server unreachable')
  with patch.object(a,'_cli_installed',return_value=True),patch.object(a.subprocess,'run',side_effect=err) as run:
   self.assertEqual(a.agy()['reason'],'source_failed');self.assertEqual(a.agy()['reason'],'source_failed');self.assertEqual(run.call_count,2)
  self.assertFalse(pathlib.Path(a._agy_reconnect_marker()).exists())
 def test_non_json_login_text_suspends_later_polls(self):
  done=subprocess.CompletedProcess(['agy'],0,stdout='Please sign in via OAuth',stderr='')
  with patch.object(a,'_cli_installed',return_value=True),patch.object(a.subprocess,'run',return_value=done) as run:
   self.assertEqual(a.agy()['reason'],'login_required');self.assertEqual(a.agy()['reason'],'refresh_paused');self.assertEqual(run.call_count,1)
 def test_json_login_error_suspends_later_polls(self):
  done=subprocess.CompletedProcess(['agy'],0,stdout='{"quota_summary_error":"Please sign in"}',stderr='')
  with patch.object(a,'_cli_installed',return_value=True),patch.object(a.subprocess,'run',return_value=done) as run:
   self.assertEqual(a.agy()['reason'],'login_required');self.assertEqual(a.agy()['reason'],'refresh_paused');self.assertEqual(run.call_count,1)
 def test_unwritable_latch_reports_retry_risk(self):
  err=subprocess.CalledProcessError(1,'agy',stderr='sign in required')
  with patch.object(a,'_cli_installed',return_value=True),patch.object(a.subprocess,'run',side_effect=err),patch.object(a,'_agy_latch_reconnect',return_value=False):
   self.assertIn('background refresh may retry',a.agy()['detail'])
 def test_transient_failure_does_not_latch(self):
  with patch.object(a,'_cli_installed',return_value=True),patch.object(a.subprocess,'run',side_effect=OSError('temporary')) as run:
   self.assertEqual(a.agy()['reason'],'source_failed');self.assertEqual(a.agy()['reason'],'source_failed');self.assertEqual(run.call_count,2)
  self.assertFalse(pathlib.Path(a._agy_reconnect_marker()).exists())
 def test_unwritable_latch_does_not_crash(self):
  with patch.object(a.os,'open',side_effect=PermissionError('denied')):self.assertFalse(a._agy_latch_reconnect())
 def test_missing_cli_does_not_latch(self):
  with patch.object(a,'_cli_installed',return_value=False):self.assertEqual(a.agy()['reason'],'cli_not_installed')
  self.assertFalse(pathlib.Path(a._agy_reconnect_marker()).exists())
 def test_connect_uses_provider_login_and_success_only_latch_clear(self):
  with patch.object(c.sys,'argv',['connect','antigravity']),patch.object(c.shutil,'which',return_value='/fake'),patch.object(c,'terminal_command',side_effect=lambda x:x),patch.object(c.subprocess,'Popen') as p:
   c.main();cmd=p.call_args.args[0][2]
  self.assertIn('agy acp --login && rm -f -- ',cmd);self.assertIn('agy-reconnect-required',cmd);self.assertNotIn('antigravity-keyring-unavailable',cmd)
 def test_simulated_keyring_timeout_detaches_tty_pauses_during_cooldown_and_recovers(self):
  root=pathlib.Path(self.tmp.name)
  fake_agy=root/'fake-agy';fake_agy.write_text(FAKE_AGY_KEYRING_SCRIPT);fake_agy.chmod(0o755)
  calls_file=root/'calls.json';oauth_file=root/'oauth.txt'
  runtime_dir=root/'runtime';runtime_dir.mkdir();(runtime_dir/'bus').touch()
  marker=root/'.gemini/antigravity-cli/cache/antigravity-keyring-unavailable'
  # Run Poll 1 from a parent process that owns a real controlling PTY (/dev/tty)
  # to prove start_new_session=True detaches /dev/tty and prevents print-mode OAuth.
  master_fd,slave_fd=pty.openpty()
  self.addCleanup(os.close,master_fd)
  def attach_ctty():
   os.setsid()
   fcntl.ioctl(slave_fd,termios.TIOCSCTTY,0)
  env=os.environ.copy()
  env.update({
   'AGENT_METER_AGY_BIN':str(fake_agy),
   'FAKE_AGY_CALLS':str(calls_file),
   'FAKE_AGY_OAUTH':str(oauth_file),
   'FAKE_KEYRING_STATE':'timeout',
   'XDG_RUNTIME_DIR':str(runtime_dir),
   'DBUS_SESSION_BUS_ADDRESS':'autolaunch:',
  })
  try:
   probe=subprocess.run(
    [sys.executable,'-c',
     f'import importlib.util, json, os; '
     f'assert os.path.exists("/dev/tty"); '
     f's=importlib.util.spec_from_file_location("a",{str(ROOT/"adapters/agent_meter_adapter.py")!r}); '
     f'm=importlib.util.module_from_spec(s); s.loader.exec_module(m); '
     f'print(json.dumps(m.agy()))'],
    env=env,stdin=slave_fd,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,check=True,
    preexec_fn=attach_ctty,
   )
  finally:
   os.close(slave_fd)
  first=json.loads(probe.stdout)
  self.assertEqual(first['status'],'unavailable')
  self.assertEqual(first['reason'],'keyring_cooldown')
  self.assertFalse(oauth_file.exists(),'agy must not open /dev/tty or start interactive OAuth')
  self.assertTrue(marker.exists())
  self.assertFalse(pathlib.Path(a._agy_reconnect_marker()).exists(),'keyring timeout must not latch permanent logout')
  calls=json.loads(calls_file.read_text())
  self.assertEqual(len(calls),1)
  self.assertTrue(calls[0]['session_leader'])
  self.assertEqual(calls[0]['dbus'],f'unix:path={runtime_dir/"bus"}')
  # Poll 2 during the 1-hour keyring cooldown skips launching agy completely.
  with patch.dict(os.environ,env):
   second=a.agy()
  self.assertEqual(second['reason'],'keyring_cooldown')
  self.assertEqual(len(json.loads(calls_file.read_text())),1)
  # Poll 3 after the 1-hour cooldown expires and the keyring recovers succeeds automatically.
  os.utime(marker,(0,0))
  env['FAKE_KEYRING_STATE']='unlocked'
  with patch.dict(os.environ,env):
   third=a.agy()
  self.assertEqual(third['status'],'fresh')
  self.assertEqual(third['windows'][0]['remaining_percent'],75.0)
  self.assertEqual(len(json.loads(calls_file.read_text())),2)
  self.assertFalse(oauth_file.exists())
 def test_keyring_marker_written_before_subprocess_timeout_reports_cooldown_without_latch(self):
  marker=pathlib.Path(self.tmp.name)/'.gemini/antigravity-cli/cache/antigravity-keyring-unavailable'
  def timeout_after_marker(*_args,**_kwargs):
   marker.parent.mkdir(parents=True,exist_ok=True);marker.touch()
   raise subprocess.TimeoutExpired('agy',20)
  with patch.object(a,'_cli_installed',return_value=True),patch.object(a.subprocess,'run',side_effect=timeout_after_marker) as run:
   self.assertEqual(a.agy()['reason'],'keyring_cooldown')
   self.assertEqual(a.agy()['reason'],'keyring_cooldown')
   self.assertEqual(run.call_count,1)
  self.assertFalse(pathlib.Path(a._agy_reconnect_marker()).exists())
 def test_subprocess_env_normalizes_missing_autolaunch_or_stale_dbus_address(self):
  runtime_dir=pathlib.Path(self.tmp.name)/'runtime';runtime_dir.mkdir();bus=runtime_dir/'bus';bus.touch()
  for raw in ('','autolaunch:',f'unix:path={self.tmp.name}/missing-bus'):
   with patch.dict(os.environ,{'XDG_RUNTIME_DIR':str(runtime_dir),'DBUS_SESSION_BUS_ADDRESS':raw}):
    env=a._agy_subprocess_env()
    self.assertEqual(env['DBUS_SESSION_BUS_ADDRESS'],f'unix:path={bus}')
  valid_bus=runtime_dir/'custom-bus';valid_bus.touch()
  with patch.dict(os.environ,{'XDG_RUNTIME_DIR':str(runtime_dir),'DBUS_SESSION_BUS_ADDRESS':f'unix:path={valid_bus}'}):
   self.assertEqual(a._agy_subprocess_env()['DBUS_SESSION_BUS_ADDRESS'],f'unix:path={valid_bus}')
if __name__=='__main__':unittest.main()
