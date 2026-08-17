#!/usr/bin/env python3
"""Validate representative specs against schemas/service.schema.json.

This exists because the schema and the Rust types are maintained by hand with
nothing connecting them, and they drifted: `spec.required` listed `image` while
`ServiceSpec` had `#[serde(default)]` on it, so every multi-container spec the
`containers[]` feature exists to write was rejected by the schema and accepted
by the parser. `cargo test` was green throughout — no test had ever fed a spec
to the schema.

Each case below asserts an expected accept/reject, so a future divergence in
either direction fails CI instead of being discovered by a user whose editor
underlines a valid file.
"""

import json
import pathlib
import sys

try:
    import jsonschema
    import yaml
except ImportError as exc:  # pragma: no cover
    print(f"SKIP: {exc} (install jsonschema and pyyaml to run this check)")
    sys.exit(0)

ROOT = pathlib.Path(__file__).resolve().parent.parent
SCHEMA = json.loads((ROOT / "schemas" / "service.schema.json").read_text())

CASES: list[tuple[str, bool, str]] = [
    (
        "single-container, minimal",
        True,
        """
apiVersion: ecsctl/v1
kind: Service
metadata: {name: app, cluster: prod}
spec: {image: nginx:latest, cpu: "256", memory: "512"}
""",
    ),
    (
        "single-container with the flat optional fields",
        True,
        """
apiVersion: ecsctl/v1
kind: Service
metadata: {name: app, cluster: prod}
spec:
  image: nginx:latest
  cpu: "512"
  memory: "1024"
  port: 8080
  command: ["sh", "-c", "exec nginx"]
  execEnabled: true
  logGroup: /ecs/app
  env: {LOG_LEVEL: info}
  secrets: {TOKEN: "arn:aws:secretsmanager:us-east-1:1:secret:t"}
  subnets: [subnet-0123456789abcdef0]
  securityGroups: [sg-0123456789abcdef0]
  assignPublicIp: true
""",
    ),
    (
        "sidecars: init container ordering plus a shared volume",
        True,
        """
apiVersion: ecsctl/v1
kind: Service
metadata: {name: app, cluster: prod}
spec:
  cpu: "1024"
  memory: "2048"
  volumes: [{name: workspace}]
  containers:
    - name: init-perms
      image: app:latest
      essential: false
      user: "0"
      entryPoint: ["/bin/sh", "-c"]
      command: ["chown 1000:1000 /workspace"]
      mountPoints: {/workspace: workspace}
    - name: app
      image: app:latest
      essential: true
      user: "1000"
      readonlyRootFilesystem: true
      dependsOn: [{containerName: init-perms, condition: SUCCESS}]
      linuxParameters: {capabilitiesDrop: ["ALL"]}
      mountPoints: {/workspace: workspace}
""",
    ),
    (
        "mountPoints: both the shorthand and the readOnly object form",
        True,
        """
apiVersion: ecsctl/v1
kind: Service
metadata: {name: app, cluster: prod}
spec:
  cpu: "1024"
  memory: "2048"
  volumes: [{name: workspace}, {name: config}]
  containers:
    - name: app
      image: app:latest
      mountPoints:
        /workspace: workspace
        /config: {sourceVolume: config, readOnly: true}
""",
    ),
    (
        "EFS volume",
        True,
        """
apiVersion: ecsctl/v1
kind: Service
metadata: {name: app, cluster: prod}
spec:
  cpu: "1024"
  memory: "2048"
  volumes:
    - name: shared
      efs:
        fileSystemId: fs-0123456789abcdef0
        rootDirectory: /data
        transitEncryption: ENABLED
  containers:
    - name: app
      image: app:latest
      mountPoints: {/data: shared}
""",
    ),
    (
        "neither image nor containers",
        False,
        """
apiVersion: ecsctl/v1
kind: Service
metadata: {name: app, cluster: prod}
spec: {cpu: "256", memory: "512"}
""",
    ),
    (
        "misspelled readOnly in the detailed mount form",
        False,
        """
apiVersion: ecsctl/v1
kind: Service
metadata: {name: app, cluster: prod}
spec:
  cpu: "1024"
  memory: "2048"
  volumes: [{name: config}]
  containers:
    - name: app
      image: app:latest
      mountPoints:
        /config: {sourceVolume: config, readonly: true}
""",
    ),
    (
        "capability name outside the ECS set",
        False,
        """
apiVersion: ecsctl/v1
kind: Service
metadata: {name: app, cluster: prod}
spec:
  cpu: "1024"
  memory: "2048"
  containers:
    - name: app
      image: app:latest
      linuxParameters: {capabilitiesDrop: ["SYS_ADM"]}
""",
    ),
    (
        "unknown field anywhere in the spec",
        False,
        """
apiVersion: ecsctl/v1
kind: Service
metadata: {name: app, cluster: prod}
spec: {image: nginx:latest, cpu: "256", memory: "512", notAThing: 1}
""",
    ),
    (
        "cpu outside the Fargate set",
        False,
        """
apiVersion: ecsctl/v1
kind: Service
metadata: {name: app, cluster: prod}
spec: {image: nginx:latest, cpu: "300", memory: "512"}
""",
    ),
]


def main() -> int:
    failures = 0
    for label, should_pass, doc in CASES:
        try:
            jsonschema.validate(yaml.safe_load(doc), SCHEMA)
            ok = True
            detail = ""
        except jsonschema.ValidationError as exc:
            ok = False
            detail = exc.message

        if ok == should_pass:
            print(f"  ok       {label}")
        else:
            failures += 1
            expected = "accept" if should_pass else "reject"
            print(f"  FAILED   {label}: expected schema to {expected}")
            if detail:
                print(f"           {detail}")

    print()
    if failures:
        print(f"{failures} schema case(s) failed")
        return 1
    print(f"all {len(CASES)} schema cases behaved as expected")
    return 0


if __name__ == "__main__":
    sys.exit(main())
