#!/usr/bin/env bash
# Launch a dedicated AWS instance, run the publishable benchmark suites,
# copy results back, and terminate.
#
#   ./bench/aws-run.sh
#   KEEP=1 ./bench/aws-run.sh          # leave the instance running
#   INSTANCE_TYPE=c7i.2xlarge ./bench/aws-run.sh
#
# Designed for a quiet box: no other uploads, compute-optimized Intel with
# GFNI (c7i = Sapphire Rapids). Default region us-east-1.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REGION="${AWS_REGION:-${AWS_DEFAULT_REGION:-us-east-1}}"
# Default 2xlarge (8 vCPU) stays under a typical new-account 16-vCPU cap.
# Use INSTANCE_TYPE=c7i.4xlarge when the quota allows it (16 vCPU / 8 cores).
INSTANCE_TYPE="${INSTANCE_TYPE:-c7i.2xlarge}"
VOLUME_GIB="${VOLUME_GIB:-50}"
KEY_NAME="${KEY_NAME:-pesto-bench-ed25519}"
SG_NAME="${SG_NAME:-pesto-gfni-bench-sg}"
NAME_TAG="${NAME_TAG:-pesto-bench-clean}"
KEEP="${KEEP:-0}"
SSH_USER=ubuntu
# Suites/workloads that close issue #147 (yenc + par2 + e2e, headline corpora).
BENCH_CMD="${BENCH_CMD:-./bench/run.sh yenc par2 e2e --workload many-small --workload mixed-folder --workload movie-1080p --yes}"

AMI_PARAM="/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id"

die() { echo "error: $*" >&2; exit 1; }
info() { printf '── %s\n' "$*"; }

need() { command -v "$1" >/dev/null || die "need $1 on PATH"; }
need aws
need ssh
need rsync
need curl

aws sts get-caller-identity --region "$REGION" >/dev/null \
    || die "aws credentials not working in $REGION"

MY_IP=$(curl -fsS --max-time 8 https://checkip.amazonaws.com | tr -d '[:space:]')
[[ $MY_IP =~ ^[0-9.]+$ ]] || die "could not detect public IP"

info "region=$REGION type=$INSTANCE_TYPE from=$MY_IP"

# ── key pair (this machine's ed25519) ────────────────────────────────────────
if ! aws ec2 describe-key-pairs --region "$REGION" --key-names "$KEY_NAME" \
        >/dev/null 2>&1; then
    info "importing $HOME/.ssh/id_ed25519.pub as $KEY_NAME"
    aws ec2 import-key-pair --region "$REGION" --key-name "$KEY_NAME" \
        --public-key-material "fileb://$HOME/.ssh/id_ed25519.pub" >/dev/null
fi

# ── security group: SSH from this IP only ────────────────────────────────────
VPC=$(aws ec2 describe-vpcs --region "$REGION" \
    --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)
[[ $VPC != None && -n $VPC ]] || die "no default VPC in $REGION"

SG=$(aws ec2 describe-security-groups --region "$REGION" \
    --filters Name=group-name,Values="$SG_NAME" Name=vpc-id,Values="$VPC" \
    --query 'SecurityGroups[0].GroupId' --output text)
if [[ $SG == None || -z $SG ]]; then
    info "creating security group $SG_NAME"
    SG=$(aws ec2 create-security-group --region "$REGION" --vpc-id "$VPC" \
        --group-name "$SG_NAME" --description "pesto bench SSH" \
        --query GroupId --output text)
fi
if ! aws ec2 describe-security-groups --region "$REGION" --group-ids "$SG" \
        --query "SecurityGroups[0].IpPermissions[?FromPort==\`22\`].IpRanges[].CidrIp" \
        --output text | tr '\t' '\n' | grep -qx "${MY_IP}/32"; then
    info "authorizing tcp/22 from ${MY_IP}/32"
    aws ec2 authorize-security-group-ingress --region "$REGION" --group-id "$SG" \
        --protocol tcp --port 22 --cidr "${MY_IP}/32" >/dev/null
fi

# Prefer an AZ that actually offers this instance type.
AZ=$(aws ec2 describe-instance-type-offerings --region "$REGION" \
    --location-type availability-zone \
    --filters Name=instance-type,Values="$INSTANCE_TYPE" \
    --query 'InstanceTypeOfferings[0].Location' --output text)
SUBNET=$(aws ec2 describe-subnets --region "$REGION" \
    --filters Name=vpc-id,Values="$VPC" Name=availability-zone,Values="$AZ" \
    --query 'Subnets[0].SubnetId' --output text)
[[ $SUBNET != None && -n $SUBNET ]] || die "no subnet in $AZ"

AMI=$(aws ssm get-parameters --region "$REGION" --names "$AMI_PARAM" \
    --query 'Parameters[0].Value' --output text)
[[ -n $AMI && $AMI != None ]] || die "could not resolve Ubuntu 24.04 AMI"

info "launching $INSTANCE_TYPE ami=$AMI az=$AZ"

IID=$(aws ec2 run-instances --region "$REGION" \
    --image-id "$AMI" --instance-type "$INSTANCE_TYPE" \
    --key-name "$KEY_NAME" --security-group-ids "$SG" --subnet-id "$SUBNET" \
    --associate-public-ip-address \
    --block-device-mappings "DeviceName=/dev/sda1,Ebs={VolumeSize=${VOLUME_GIB},VolumeType=gp3,Throughput=250,Iops=3000,DeleteOnTermination=true}" \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=${NAME_TAG}}]" \
    --metadata-options HttpTokens=required,HttpEndpoint=enabled \
    --query 'Instances[0].InstanceId' --output text)
echo "  instance $IID"

cleanup() {
    local rc=$?
    if [[ ${KEEP} == 1 ]]; then
        echo "KEEP=1 — leaving $IID running"
        return
    fi
    if [[ -n ${IID:-} ]]; then
        info "terminating $IID"
        aws ec2 terminate-instances --region "$REGION" --instance-ids "$IID" >/dev/null || true
    fi
    exit "$rc"
}
trap cleanup EXIT

info "waiting for running + status ok"
aws ec2 wait instance-running --region "$REGION" --instance-ids "$IID"
aws ec2 wait instance-status-ok --region "$REGION" --instance-ids "$IID"

IP=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$IID" \
    --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "  public $IP"

SSH=(ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10
     -o ServerAliveInterval=30 -i "$HOME/.ssh/id_ed25519" "${SSH_USER}@${IP}")

for i in $(seq 1 30); do
    if "${SSH[@]}" true 2>/dev/null; then
        break
    fi
    sleep 5
    [[ $i == 30 ]] && die "ssh never came up on $IP"
done

info "syncing tree (no target/, corpora, results, media)"
rsync -az --delete --info=stats1 \
    --exclude '/target/' \
    --exclude '/bench/data/' \
    --exclude '/bench/results/' \
    --exclude '/node_modules/' \
    --exclude '/.git/' \
    --exclude '*.mkv' --exclude '*.nzb' --exclude '*.par2' \
    --exclude 'Terapia*/' --exclude 'test_results/' \
    -e "ssh -o StrictHostKeyChecking=accept-new -i $HOME/.ssh/id_ed25519" \
    "$ROOT/" "${SSH_USER}@${IP}:pesto/"

info "installing toolchain + competitors on the instance"
"${SSH[@]}" bash -s <<'REMOTE'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
export NEEDRESTART_MODE=a
# Regional EC2 Ubuntu mirrors have been stalling (minutes of Ign: retries).
# archive.ubuntu.com is the Cloudflare CDN and has been reliable from us-east-1.
if [[ -f /etc/apt/sources.list.d/ubuntu.sources ]]; then
    sudo sed -i 's|http://[a-z0-9-]*\.ec2\.archive\.ubuntu\.com/ubuntu/|http://archive.ubuntu.com/ubuntu/|' \
        /etc/apt/sources.list.d/ubuntu.sources || true
fi
sudo apt-get -o Acquire::Retries=5 update -qq
sudo apt-get -o Acquire::Retries=5 install -y \
    build-essential pkg-config libssl-dev cmake git curl ca-certificates \
    rsync python3 time par2 linux-tools-common linux-tools-generic \
    cpufrequtils >/dev/null

# Node 22 for parpar / nyuu / node-yencode
if ! command -v node >/dev/null || ! node -v | grep -qE 'v2[2-9]'; then
    curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
    sudo apt-get install -y -qq nodejs >/dev/null
fi
sudo npm install -g @animetosho/parpar nyuu >/dev/null

# rustup stable. `minimal` has no cargo — we need default (rustc+std+cargo).
if ! command -v cargo >/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile default
fi
# shellcheck disable=SC1091
. "$HOME/.cargo/env"
command -v cargo
cargo --version

# Prefer performance governor when the driver exposes it (Nitro often already is).
if command -v cpupower >/dev/null; then
    sudo cpupower frequency-set -g performance >/dev/null 2>&1 || true
fi
echo "governor=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo none)"
echo "model=$(grep -m1 'model name' /proc/cpuinfo)"
echo "flags=$(grep -m1 '^flags' /proc/cpuinfo | tr ' ' '\n' | grep -E '^(avx2|avx512f|gfni)$' | xargs)"
nproc
free -h | head -2
REMOTE

# Expand $HOME on the remote. rustup's env file is not safe to `source` over
# a non-login ssh (`source: filename argument required` when CARGO_HOME is unset).
remote() {
    "${SSH[@]}" "export PATH=\"\$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\"; $*"
}

info "building release binaries"
remote 'cd pesto && cargo build --release --bins --examples -p pesto-poster -p parmesan-par2'

info "node-yencode (local, for the yEnc micro table)"
remote 'cd pesto && npm install --no-fund --no-audit yencode >/dev/null'

info "running: $BENCH_CMD"
# Unbuffered so a dropped session still leaves logs on the instance.
remote "cd pesto && $BENCH_CMD" | tee "/tmp/pesto-aws-bench-${IID}.log"

info "fetching results"
mkdir -p "$ROOT/bench/results"
rsync -az --info=stats1 \
    -e "ssh -o StrictHostKeyChecking=accept-new -i $HOME/.ssh/id_ed25519" \
    "${SSH_USER}@${IP}:pesto/bench/results/" "$ROOT/bench/results/"

info "done — results under bench/results/ (hostname of the instance)"
ls -ltd "$ROOT/bench/results"/*/ | head -5
