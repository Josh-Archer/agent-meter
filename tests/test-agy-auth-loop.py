"""No live credentials, login, browser, or provider calls."""
import importlib.util
import os
import pathlib
import subprocess
import tempfile
import unittest
from unittest.mock import patch
ROOT=pathlib.Path(__file__).resolve().parents[1]
def load(name):
 s=importlib.util.spec_from_file_location(name, ROOT/'adapters'/f'{name}.py');m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
a=load('agent_meter_adapter');c=load('agent_meter_connect')
class AuthLoop(unittest.TestCase):
 def setUp(self):
  self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
  self.env=patch.dict(os.environ,{'HOME':self.tmp.name,'XDG_STATE_HOME':self.tmp.name+'/state'});self.env.start();self.addCleanup(self.env.stop)
 def test_provider_marker_prevents_launch_without_reading_credentials(self):
  marker=pathlib.Path(self.tmp.name)/'.gemini/antigravity-cli/cache/antigravity-keyring-unavailable';marker.parent.mkdir(parents=True);marker.touch()
  with patch.object(a,'_cli_installed',return_value=True),patch.object(a.subprocess,'run') as run:
   self.assertEqual(a.agy()['reason'],'keyring_cooldown');run.assert_not_called()
  self.assertTrue(marker.exists())
 def test_timeout_latches_subsequent_polls(self):
  with patch.object(a,'_cli_installed',return_value=True),patch.object(a.subprocess,'run',side_effect=subprocess.TimeoutExpired('agy',20)) as run:
   self.assertEqual(a.agy()['reason'],'timeout');self.assertEqual(a.agy()['reason'],'refresh_paused');self.assertEqual(run.call_count,1)
   self.assertEqual(run.call_args.kwargs['stdin'],subprocess.DEVNULL);self.assertLess(run.call_args.kwargs['timeout'],30)
  self.assertEqual(pathlib.Path(a._agy_reconnect_marker()).read_text(),'reconnect required\n')
 def test_expired_marker_allows_retry(self):
  marker=pathlib.Path(self.tmp.name)/'.gemini/antigravity-cli/cache/antigravity-keyring-unavailable';marker.parent.mkdir(parents=True);marker.touch();os.utime(marker,(0,0))
  self.assertIsNone(a._agy_pause_reason());self.assertTrue(marker.exists())
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
if __name__=='__main__':unittest.main()
