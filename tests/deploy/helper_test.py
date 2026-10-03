import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]


class DeploymentHelperTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="kanata-helper-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.config_dir = self.root / "operator config"
        self.config_dir.mkdir()
        self.config = self.config_dir / "config.toml"
        self.auth_dir = self.root / "private auth"
        self.output = self.root / "arguments.json"
        bins = self.root / "bin"
        bins.mkdir()
        self.checkout = self.root / "checkout"
        (self.checkout / "scripts").mkdir(parents=True)
        self.helper = self.checkout / "scripts/kanata.sh"
        self.helper.write_text((REPO / "scripts/kanata.sh").read_text())
        (bins / "docker").write_text(
            "#!/usr/bin/env python3\nimport os, sys\n"
            'assert sys.argv[1:] == ["compose", "config", "--format", "json"], sys.argv\n'
            'print(os.environ["KANATA_HELPER_MODEL"])\n'
        )
        (bins / "kanata").write_text(
            "#!/usr/bin/env python3\nimport json, os, sys\n"
            'with open(os.environ["KANATA_HELPER_OUTPUT"], "w") as f:\n'
            "    json.dump(sys.argv[1:], f)\n"
        )
        for executable in bins.iterdir():
            executable.chmod(0o700)
        self.env = os.environ | {
            "PATH": str(bins) + os.pathsep + os.environ["PATH"],
            "KANATA_HELPER_OUTPUT": str(self.output),
        }
        self.env.pop("KANATA_IMAGE", None)
        self.model = {"services": {"kanata": {"volumes": [
            self.mount(self.config, "/etc/kanata/config.toml"),
            self.mount(self.config_dir / "keys", "/etc/kanata/keys"),
            self.mount(self.config_dir / "state/private", "/etc/kanata/state"),
        ]}}}
        self.write_config()

    @staticmethod
    def mount(source, target):
        return {"type": "bind", "source": str(source), "target": str(target)}

    def write_config(self, auth=False):
        content = '[keys]\nfile = "keys/keys.toml"\nusage_dir = "state"\n'
        if auth:
            content += "[chatgpt_auth]\nstate_dir = " + json.dumps(str(self.auth_dir)) + "\n"
        self.config.write_text(content)

    def enable_auth(self):
        self.write_config(auth=True)
        self.model["services"]["kanata"]["volumes"].append(
            self.mount(self.auth_dir, self.auth_dir)
        )

    def run_helper(self, *args, success=True):
        self.output.unlink(missing_ok=True)
        result = subprocess.run(
            ["bash", str(self.helper), *args],
            env=self.env | {"KANATA_HELPER_MODEL": json.dumps(self.model)},
            capture_output=True, text=True,
        )
        if success:
            self.assertEqual(result.returncode, 0, result.stderr)
            return json.loads(self.output.read_text())
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.output.exists(), "rejected request reached the host binary")
        return result.stderr

    def test_portal_shared_deployment_paths_and_host_only_options(self):
        self.assertEqual(self.run_helper("portal"), ["portal", "--config", str(self.config)])
        self.assertEqual(self.run_helper("portal", "--port", "9092"),
                         ["portal", "--config", str(self.config), "--port", "9092"])
        for name in ("keys", "state", "state/private", "state/public"):
            self.assertEqual((self.config_dir / name).stat().st_mode & 0o777, 0o700)
        for args in (("--bind", "0.0.0.0"), ("--config", str(self.config)), ("--port",)):
            self.assertIn("usage: portal", self.run_helper("portal", *args, success=False))
        self.model["services"]["kanata"]["volumes"][1]["source"] = str(self.root / "other-keys")
        self.assertIn("KANATA_KEYS_DIR", self.run_helper("portal", success=False))

    def test_chatgpt_host_commands_and_profile(self):
        self.enable_auth()
        for action in ("login", "status", "logout", "models"):
            self.assertEqual(self.run_helper("chatgpt", action),
                             ["auth", "chatgpt", action, "--config", str(self.config)])
        self.assertEqual(self.auth_dir.stat().st_mode & 0o777, 0o700)
        for action in ("login", "logout"):
            self.assertEqual(self.run_helper("chatgpt", action, "--profile", "work"),
                             ["auth", "chatgpt", action, "--config", str(self.config), "--profile", "work"])
        for args in (("status", "--profile", "work"), ("login", "--config", "other.toml")):
            self.assertIn("--profile", self.run_helper("chatgpt", *args, success=False))

    def test_chatgpt_requires_writable_same_path_bind(self):
        self.write_config(auth=True)
        self.assertIn("compose.kanata.chatgpt.yml", self.run_helper("chatgpt", "status", success=False))
        volumes = self.model["services"]["kanata"]["volumes"]
        volumes.append(self.mount(self.auth_dir, "/container-only"))
        self.assertIn("matching", self.run_helper("chatgpt", "status", success=False))
        volumes[-1] = self.mount(self.auth_dir, self.auth_dir) | {"read_only": True}
        self.assertIn("matching", self.run_helper("chatgpt", "status", success=False))
        self.assertFalse(self.auth_dir.exists())

    def test_chatgpt_rejects_public_mount_and_unsafe_storage(self):
        self.enable_auth()
        self.auth_dir.mkdir(mode=0o750)
        self.assertIn("mode 0700", self.run_helper("chatgpt", "status", success=False))
        self.auth_dir.chmod(0o700)
        self.model["services"]["kanata-public"] = {"volumes": [self.mount(self.auth_dir, self.auth_dir)]}
        self.assertIn("kanata-public", self.run_helper("chatgpt", "status", success=False))
        del self.model["services"]["kanata-public"]
        target = self.root / "actual auth"
        self.auth_dir.rename(target)
        self.auth_dir.symlink_to(target, target_is_directory=True)
        self.assertIn("symlinks", self.run_helper("chatgpt", "status", success=False))
        self.auth_dir = self.checkout / "private-auth-fixture"
        self.write_config(auth=True)
        self.assertIn("outside the checkout", self.run_helper("chatgpt", "status", success=False))


if __name__ == "__main__":
    unittest.main()
