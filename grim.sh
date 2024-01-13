eval "$(ssh-agent -s)"
ssh-add id_rsa_grim
ssh -i id_rsa_grim -t root@ur_box.local 'touch /root/.death_touch; ls -la /root/.death_touch'
ssh-agent -k
