---
name: d2
description: Draw diagrams (architecture, flows, sequences, ER, class) from D2 text and render them to SVG, PNG, PDF, PPTX or ASCII with the d2 CLI. Use when asked for a diagram, chart of components, or a picture of how parts connect.
---

# d2

`d2` compiles a `.d2` text file into a diagram. It is installed and needs no network to render.

## Render

```sh
d2 diagram.d2 diagram.svg     # the output extension picks the format
d2 diagram.d2 diagram.png     # also .pdf, .pptx, .gif, .txt (ASCII)
d2 --layout=elk diagram.d2 out.svg    # layout engines: dagre (default), elk
d2 --theme=200 diagram.d2 out.svg     # `d2 themes` lists theme ids
d2 --sketch diagram.d2 out.svg        # hand-drawn look
```

Check before rendering, and keep files tidy:

```sh
d2 validate diagram.d2
d2 fmt diagram.d2
```

Do not use `d2 --watch` or `d2 play`: both open a browser or a web service.

## Syntax

```d2
direction: right

api: API server
db: Postgres {shape: cylinder}
queue: Jobs {shape: queue}

api -> db: reads and writes
api -> queue: enqueue
queue -> api: results {style.stroke-dash: 3}

backend: Backend {
  api
  worker
  worker -> api
}
```

- `a -> b: label` draws a labelled edge. `<-`, `<->` and `--` also work.
- `name: Label` sets a shape's label; `name.shape: cylinder` or `{shape: ...}` sets its shape. Shapes include `rectangle`, `oval`, `circle`, `cylinder`, `queue`, `person`, `cloud`, `diamond`, `hexagon`, `page`, `document`.
- `{ ... }` after a name nests shapes inside it. Refer to a nested shape as `backend.api`.
- `style.fill`, `style.stroke`, `style.stroke-dash`, `style.font-size` set styles.
- `shape: sql_table` with `column: type` lines draws a table; `shape: class` draws a class.
- `shape: sequence_diagram` on a container turns its edges into a sequence diagram, in order.
- `# comment` lines are ignored.

Write the `.d2` source next to the rendered file, so the diagram can be edited later.
