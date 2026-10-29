# Происхождение SDK stdio ↔ WS adapter

Canonical implementation перенесена из GBIG PAM scripts/testpilot-adapter29.10.2026. Origin TASK-225, commit616310dd и последующие approved full16 изменения; перенос включает фактический provider.py с предсуществующей delta59+/1−. Private runtime config/data не включены. Каталог сохраняет действующую лицензию repository [AGPLv3](../../LICENSE); новый copyright owner не объявляется.

Внешние зависимости (source не vendored):

- [ROCTUP/1c-testpilot](https://github.com/ROCTUP/1c-testpilot) — отдельный SDK/backend, AGPL; его notices/авторство сохраняются.
- [Official MCP Python SDK](https://github.com/modelcontextprotocol/python-sdk) — mcp==1.30.0, MIT; публичные ClientSession/stdio streams.
- [1c-debug-mcp](https://github.com/PavRedAlex/1c-debug-mcp) — отдельный Node SDK, MIT согласно package.json; canonical build остаётся в своём repo.
- jsonschema/websockets — внешние dependencies requirements.txt.

Другой проект ROCTUP/1c-mcp-toolkit GPL-3.0/native source в перенос не входит.
