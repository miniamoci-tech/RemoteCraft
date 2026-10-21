# Running the RemoteCraft relay on Linux

## 1. Build or copy the binary

On the Linux host, with Rust installed:

```bash
git clone https://github.com/miniamoci-tech/RemoteCraft
cd RemoteCraft
cargo build -p remotecraft-relay --release
sudo install -m 0755 target/release/remotecraft-relay /usr/local/bin/
```

(Or cross-compile elsewhere and copy the `remotecraft-relay` binary to
`/usr/local/bin/`.)

## 2. Create a service user and install the unit

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin remotecraft
sudo cp deploy/linux/remotecraft-relay.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now remotecraft-relay
systemctl status remotecraft-relay
```

## 3. Open the port

The relay listens on TCP **21118** by default. Allow it through the firewall:

```bash
# ufw
sudo ufw allow 21118/tcp
# firewalld
sudo firewall-cmd --permanent --add-port=21118/tcp && sudo firewall-cmd --reload
```

If the box is behind a router, forward TCP 21118 to it. Both desktop peers will
be pointed at this server's public address and port.

## 4. Change the port (optional)

Edit `ExecStart` in the unit (`--bind 0.0.0.0:PORT`), then
`sudo systemctl daemon-reload && sudo systemctl restart remotecraft-relay`.

## Notes

- The relay forwards only opaque, end-to-end-encrypted bytes between paired
  peers. It stores nothing and logs no session contents.
- `RUST_LOG=debug` in the unit's `Environment=` increases log verbosity.
