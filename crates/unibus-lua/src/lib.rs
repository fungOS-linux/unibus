//! Install into a host-owned VM; no network-thread Lua callbacks.
//! LuaJIT is the default backend; select Luau explicitly for existing Canvas hosts.
use mlua::{Lua, LuaSerdeExt, UserData, UserDataMethods, Value};
use unibus_sdk::{Publish, Session};

struct LuaSession(Session);

/// Expose only a host-selected session to a sandbox; do not install connect there.
pub fn attach(lua: &Lua, name: &str, session: Session) -> mlua::Result<()> {
    lua.globals().set(name, LuaSession(session))
}
impl UserData for LuaSession {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("publish", |lua, this, value: Value| {
            let command: Publish = lua.from_value(value)?;
            this.0.publish(command).map_err(mlua::Error::external)
        });
        methods.add_method("poll", |lua, this, ()| match this.0.poll() {
            Some(event) => lua.to_value(&event),
            None => Ok(Value::Nil),
        });
        methods.add_method("status", |lua, this, ()| lua.to_value(&this.0.status()));
    }
}
pub fn install(lua: &Lua) -> mlua::Result<()> {
    let module = lua.create_table()?;
    module.set(
        "connect",
        lua.create_function(|lua, config: Value| {
            Session::connect(lua.from_value(config)?)
                .map(LuaSession)
                .map_err(mlua::Error::external)
        })?,
    )?;
    lua.globals().set("unibus", module)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "luajit")]
    #[test]
    fn luajit_compiles_a_hot_loop() {
        let lua = Lua::new();
        install(&lua).unwrap();
        lua.load(
            r#"
            assert(type(jit) == "table")
            assert(jit.version:match("LuaJIT"))
            assert(jit.status(), "LuaJIT compiler is disabled")
            jit.flush()
            jit.opt.start("hotloop=1")
            local sum = 0
            for i = 1, 10000 do sum = sum + i end
            assert(sum == 50005000)
            assert(require("jit.util").traceinfo(1), "hot loop did not produce a JIT trace")
        "#,
        )
        .exec()
        .unwrap();
    }
    #[test]
    fn attached_session_polls_nil_and_correlates_publish() {
        use std::{
            io::Write,
            net::TcpListener,
            thread,
            time::{Duration, Instant},
        };
        use unibus::{Envelope, Message, framing, kinds};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut token = tempfile::NamedTempFile::new().unwrap();
        token.write_all(b"host-secret").unwrap();
        let config = serde_json::from_value(serde_json::json!({
            "installation_id":"test", "producer_id":"lua", "zones":["test"],
            "hub_endpoint":listener.local_addr().unwrap().to_string(), "token_file":token.path()
        }))
        .unwrap();
        let router = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (registration, _) = framing::read_frame(&mut socket).unwrap();
            let reply = Envelope::new(
                "test",
                "router",
                Some("lua"),
                "host-secret",
                Message::new(
                    kinds::V1_REPLY,
                    serde_json::json!({"request_id":registration.request_id,"success":true}),
                ),
            );
            framing::write_frame(&mut socket, &reply, &[]).unwrap();
            loop {
                let (command, _) = framing::read_frame(&mut socket).unwrap();
                if command.message.kind != "test.command.v1" {
                    continue;
                }
                let event = Envelope::new(
                    "test",
                    "source",
                    Some("lua"),
                    "host-secret",
                    Message::new(
                        "test.reply.v1",
                        serde_json::json!({"request_id":command.request_id}),
                    ),
                );
                framing::write_frame(&mut socket, &event, &[]).unwrap();
                break;
            }
        });
        let lua = Lua::new();
        attach(&lua, "bus", Session::connect(config).unwrap()).unwrap();
        lua.load(
            r#"
            assert(unibus == nil)
            assert(bus:poll() == nil)
            request_id = bus:publish({kind="test.command.v1",body={hello="world"}})
            assert(type(request_id) == "string")
        "#,
        )
        .exec()
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let done: bool = lua
                .load(
                    r#"
                local event = bus:poll()
                if not event then return false end
                assert(event.kind == "test.reply.v1")
                assert(event.body.request_id == request_id)
                assert(event.token == nil)
                assert(bus:poll() == nil)
                return true
            "#,
                )
                .eval()
                .unwrap();
            if done {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        router.join().unwrap();
    }
    #[test]
    fn installs_and_rejects_invalid_config() {
        let lua = Lua::new();
        install(&lua).unwrap();
        assert!(lua.load("return unibus.connect({})").exec().is_err());
    }
}
