# Exact capability route, not the generic stream-interest fan-out.
def node($id; $cap; $kinds):
  {producer_id:$id, token_file:("/etc/unibus/"+$id+".token"),
   zones:["panopticon"], capabilities:[$cap], publishes:$kinds,
   publish_zones:["panopticon"]};
if any(.routes[]; .kind == "panopticon.wall.snapshot.v1")
   or any(.nodes[]; .producer_id == "panopticon-studio-source" or .producer_id == "panopticon-tv-presenter")
then error("Panopticon already configured; inspect existing grants instead of overwriting")
else
  .routes += [{kind:"panopticon.wall.snapshot.v1",target:{mode:"capability",name:"panopticon.wall.viewer"}}]
  | .nodes += [node("panopticon-studio-source";"panopticon.wall.source";["panopticon.wall.snapshot.v1"]),
              node("panopticon-tv-presenter";"panopticon.wall.viewer";[])]
end
