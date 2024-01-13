#!/usr/bin/env python3
# Enhanced 'grim_reaper' daemon script with robust error handling and verbose debugging.

import os
import shutil
import subprocess
import hashlib
import logging
import time
import subprocess
from datetime import datetime, timedelta

# Logging setup
log_file = '/var/log/grim_reaper.log'
logging.basicConfig(filename=log_file, format='%(asctime)s %(levelname)s: %(message)s', level=logging.DEBUG)

def calculate_file_hash(file_path):
    """
    Calculates the SHA256 hash of the file at the given path.
    """
    try:
        with open(file_path, 'rb') as file:
            return hashlib.sha256(file.read()).hexdigest()
    except Exception as e:
        logging.error(f"Error calculating file hash: {e}")
        return None

def check_file_modification(file_path):
    """
    Check the existence and last modified time of '/root/.death_touch'
    """
    try:
        last_modified_time = datetime.fromtimestamp(os.path.getmtime(file_path))
        if datetime.now() - last_modified_time > timedelta(hours=36):
            logging.debug("File modification condition met.")
            return True
        return False
    except FileNotFoundError:
        logging.warning("File not found: {}".format(file_path))
        return False
    except Exception as e:
        logging.error(f"Error checking file modification: {e}")
        return False

def shutdown_system():
    """
    Forcibly shutting down the system with REIUS sequence
    """
    try:
        sysrq_commands = [
                          #'r', 'e', 'i', 'u', 's', 
                          'o']  # REIUS sequence followed by 'o' for shutdown
        for command in sysrq_commands:
            with open('/proc/sysrq-trigger', 'w') as sysrq:
                sysrq.write(command)
                logging.debug(f"Executed sysrq command: {command}")
                time.sleep(1)  # Short delay between commands for system stability
    except Exception as e:
        logging.error(f"Error during system shutdown: {e}")

def install_self():
    """
    Checks if installation is needed and performs installation if necessary.
    """
    try:
        script_path = os.path.realpath(__file__)
        target_path = '/usr/local/bin/grim_reaper.py'
        service_path = '/etc/systemd/system/grim_reaper.service'

        current_hash = calculate_file_hash(script_path)
        if current_hash is None:
            raise Exception("Failed to calculate current script hash.")

        # Check if the script is already installed and up to date
        if os.path.exists(target_path):
            installed_hash = calculate_file_hash(target_path)
            if installed_hash is None:
                raise Exception("Failed to calculate installed script hash.")
            if current_hash == installed_hash:
                logging.debug("Script already installed and up to date.")
                return False

        # Copying the script to the target location
        shutil.copy(script_path, target_path)
        logging.debug(f"Script copied to {target_path}")

        # Creating/Updating systemd service file
        service_content = f"""
        [Unit]
        Description=Grim Reaper Daemon

        [Service]
        ExecStart=/usr/bin/python3 {target_path}

        [Install]
        WantedBy=multi-user.target
        """
        with open(service_path, 'w') as service_file:
            service_file.write(service_content)
        logging.debug("Systemd service file created/updated.")

        # Reloading and restarting the service
        subprocess.run(['systemctl', 'daemon-reload'])
        subprocess.run(['systemctl', 'enable', 'grim_reaper.service'])
        subprocess.run(['systemctl', 'start', 'grim_reaper.service'])
        logging.debug("Systemd service reloaded and restarted.")

        return True
    except Exception as e:
        logging.error(f"Error during installation: {e}")
        return False

def get_system_uptime():
    """
    Returns the system uptime in seconds by parsing the output of the 'uptime' command.
    """
    try:
        uptime_string = subprocess.check_output(['uptime', '-p']).decode('utf-8')
        # Example output: "up 2 hours, 15 minutes"

        hours, minutes = 0, 0
        if 'hour' in uptime_string:
            hours = int(uptime_string.split(' hour')[0].split(' ')[-1])
        if 'minute' in uptime_string:
            minutes = int(uptime_string.split(' minute')[0].split(' ')[-1])

        return (hours * 3600) + (minutes * 60)
    except Exception as e:
        logging.error(f"Error in getting system uptime: {e}")
        return None

def main():
    try:
        # Installing the script and updating the systemd service
        if install_self():
            logging.info("Exiting after installation. Systemd will restart the script.")
            return

        # Check system uptime
        uptime = get_system_uptime()
        if uptime is None:
            raise Exception("Failed to get system uptime.")
        if uptime < 1800:  # Less than 30 min
            logging.info(f"System uptime is less than 30 Minutes ({uptime} seconds). Delaying file check.")
            time.sleep(1800 - uptime)  # Sleep until uptime is reached

        # Main daemon functionality
        while True:
            if check_file_modification('/root/.death_touch'):
                shutdown_system()
                break
            time.sleep(420)  
    except Exception as e:
        logging.critical(f"Fatal error in main loop: {e}")

if __name__ == "__main__":
    main()

# Note: Execute this script with root privileges.

