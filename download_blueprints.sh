#!/bin/bash
# Download Nimby Rails workshop blueprints using SteamCMD
# Usage: ./download_blueprints.sh <workshop_id> [workshop_id...]
# Or: ./download_blueprints.sh --batch  (downloads a predefined list)

GAME_ID=1134710
DOWNLOAD_DIR="/Users/alex/Developer/nimby_gen/test_blueprints"
mkdir -p "$DOWNLOAD_DIR"

download_item() {
    local WID=$1
    echo "Downloading workshop item $WID..."
    steamcmd +force_install_dir "$DOWNLOAD_DIR" \
        +login anonymous \
        +workshop_download_item $GAME_ID $WID \
        +quit 2>&1 | tail -5
    
    # SteamCMD downloads to a specific path structure
    local SRC="$DOWNLOAD_DIR/steamapps/workshop/content/$GAME_ID/$WID/blueprints.nrclip"
    if [ -f "$SRC" ]; then
        cp "$SRC" "$DOWNLOAD_DIR/${WID}.nrclip"
        echo "  -> $DOWNLOAD_DIR/${WID}.nrclip"
    else
        echo "  -> FAILED (file not found)"
    fi
}

if [ "$1" = "--batch" ]; then
    # Popular Nimby Rails blueprint workshop items
    # Found by browsing the workshop
    ITEMS=(
        2949234540
        2821088974
        3012628444
        3281580158
        3404044164
        3362588498
        3330766804
        3291462524
        3189571206
        3134937762
    )
    for WID in "${ITEMS[@]}"; do
        download_item "$WID"
    done
else
    for WID in "$@"; do
        download_item "$WID"
    done
fi

echo ""
echo "Downloaded files:"
ls -la "$DOWNLOAD_DIR"/*.nrclip 2>/dev/null
